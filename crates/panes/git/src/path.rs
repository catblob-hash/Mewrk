//! `PathText`: a path that gives way in the middle when it does not fit, the way upstream's `lib/pathDisplay.ts` does.
// TODO: this and `mewrk_pane_files::links::elide_path` are the same function. Once the files pane stops needing this
// crate's neighbours, one should call the other.

use mewrk_client::prelude::*;

const ELLIPSIS: char = '…';

fn shorten_name(name: &str, keep: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if keep >= chars.len() {
        return name.to_owned();
    }
    let (start, end) = (keep.div_ceil(2), keep / 2);
    chars[..start].iter().copied().chain(std::iter::once(ELLIPSIS)).chain(chars[chars.len() - end..].iter().copied()).collect()
}

fn largest_fitting(most: usize, fits: &mut dyn FnMut(usize) -> bool) -> Option<usize> {
    if most < 1 || !fits(1) {
        return None;
    }
    let (mut low, mut high) = (1, most);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Some(low)
}

/// Splits into names and the separator runs between them, as `path.split(/([\\/]+)/)` does: odd indices are separators.
fn tokens(path: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut in_sep = false;
    for c in path.chars() {
        let sep = c == '/' || c == '\\';
        if sep != in_sep {
            out.push(String::new());
            in_sep = sep;
        }
        out.last_mut().expect("never empty").push(c);
    }
    if in_sep {
        out.push(String::new());
    }
    out
}

/// The longest shortening of `path` that `fits`. When nothing does, the shortest form there is.
pub fn elide_path(path: &str, fits: &mut dyn FnMut(&str) -> bool) -> String {
    if fits(path) {
        return path.to_owned();
    }
    let tokens = tokens(path);
    let named: Vec<usize> = tokens.iter().enumerate().filter(|(i, t)| i % 2 == 0 && !t.is_empty()).map(|(i, _)| i).collect();
    if named.is_empty() {
        return path.to_owned();
    }
    let count = named.len();
    let compose = |head: usize, tail: usize, replaced: &[(usize, String)]| -> String {
        let part = |from: usize, to: usize| -> String { (from..to).map(|i| replaced.iter().find(|(r, _)| *r == i).map_or(tokens[i].as_str(), |(_, s)| s.as_str())).collect() };
        if head + tail >= count {
            return part(0, tokens.len());
        }
        let head_end = named[head - 1];
        let tail_start = named[count - tail];
        format!("{}{}{ELLIPSIS}{}{}", part(0, head_end + 1), tokens[head_end + 1], tokens[tail_start - 1], part(tail_start, tokens.len()))
    };
    let (mut head, mut tail) = if count >= 3 { (1, 1) } else { (count, 0) };
    if count >= 3 && fits(&compose(head, tail, &[])) {
        loop {
            let mut grew = false;
            if head + tail + 1 < count && fits(&compose(head, tail + 1, &[])) {
                tail += 1;
                grew = true;
            }
            if head + tail + 1 < count && fits(&compose(head + 1, tail, &[])) {
                head += 1;
                grew = true;
            }
            if !grew {
                return compose(head, tail, &[]);
            }
        }
    }
    let (first, last) = (named[0], named[count - 1]);
    let mut replaced: Vec<(usize, String)> = Vec::new();
    let shorten = |index: usize, replaced: &mut Vec<(usize, String)>, fits: &mut dyn FnMut(&str) -> bool| -> bool {
        let name = tokens[index].clone();
        let keep = largest_fitting(name.chars().count().saturating_sub(1), &mut |candidate| {
            let mut trial = replaced.clone();
            trial.push((index, shorten_name(&name, candidate)));
            fits(&compose(head, tail, &trial))
        });
        replaced.push((index, shorten_name(&name, keep.unwrap_or(1))));
        keep.is_some()
    };
    if first != last && shorten(first, &mut replaced, fits) {
        return compose(head, tail, &replaced);
    }
    shorten(last, &mut replaced, fits);
    compose(head, tail, &replaced)
}

/// A path, fitted to its box, with `prefix` kept whole in front. It needs a width from outside.
pub fn path_text(path: impl Into<Arc<str>>, prefix: impl Into<Arc<str>>, ty: TypeStyle, color: Option<Color>) -> E {
    let (path, prefix): (Arc<str>, Arc<str>) = (path.into(), prefix.into());
    let measured = format!("{prefix}{path}");
    let said = a11y_active().then(|| measured.clone());
    let leaf = custom::<App>(move |_, ui, r| {
        let room = r.w - ui.measure(&prefix, &ty) - 0.5;
        let shown = elide_path(&path, &mut |text| ui.measure(text, &ty) <= room);
        let y = r.y + (r.h - ty.line) * 0.5;
        ui.label(r.x, y, &format!("{prefix}{shown}"), &ty, color.unwrap_or(ui.theme.text_primary));
    })
    .measure(move |_, ui, known, avail| (known.unwrap_or_else(|| ui.measure(&measured, &ty).min(avail)), ty.line))
    .min_w(0.0);
    match said {
        Some(words) => leaf.aria_label(words),
        None => leaf,
    }
}

#[cfg(test)]
mod tests {
    use super::elide_path;

    fn fit(path: &str, width: usize) -> String {
        elide_path(path, &mut |text| text.chars().count() <= width)
    }

    #[test]
    fn a_path_gives_way_in_the_middle_a_directory_at_a_time() {
        let path = "/Users/me/Desktop/projects/mewrk";
        assert_eq!(fit(path, 100), path);
        assert_eq!(fit(path, 24), "/Users/…/projects/mewrk");
        assert_eq!(fit(path, 14), "/Users/…/mewrk");
        assert_eq!(fit(path, 13).chars().count(), 13);
        assert!(fit(path, 13).ends_with("/…/mewrk"));
    }

    #[test]
    fn windows_paths_keep_their_separators() {
        assert_eq!(fit("C:\\Users\\me\\work\\mewrk", 16), "C:\\…\\work\\mewrk");
    }
}
