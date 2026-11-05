//! `spellCheck` on a textarea: which words of a text are misspelt, with every answer kept, so typing never asks about a word twice.
//!
//! On a Mac `NSSpellChecker` answers. It is asked on a thread of its own, about finished words only, and a frame just reads what has
//! come back. Elsewhere nothing answers: a field draws no squiggle and has no guesses to offer.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

/// What answers for spelling. One thread asks it, so it may take its time.
pub trait Speller: Send {
    /// Which of `words` are misspelt. They come from `paragraph`, which says what language they are likely in.
    fn misspelt(&mut self, paragraph: &str, words: &[String]) -> Vec<String>;
    /// What might have been meant, best first.
    fn guesses(&mut self, word: &str) -> Vec<String>;
    /// Adds the word to the user's own dictionary, for good.
    fn learn(&mut self, word: &str);
    /// Stops flagging the word for as long as the application runs.
    fn ignore(&mut self, word: &str);
}

enum Job {
    Check { paragraph: String, words: Vec<String> },
    Guess(String),
    Learn(String),
    Ignore(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    /// Out with the checker.
    Asked,
    Good,
    Misspelt,
}

#[derive(Default)]
struct Known {
    verdicts: HashMap<String, Verdict>,
    /// `None` while the checker is out asking.
    guesses: HashMap<String, Option<Vec<String>>>,
}

/// More words than anyone types: past this the answers start over, so a long run of pastes cannot grow them for ever.
const KNOWN_WORDS: usize = 50_000;

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);
/// Moves whenever an answer does, which is what tells a field that what it worked out is out of date.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The outer `None` is a checker not looked for yet; the inner one, a platform that has none.
static CHECKER: Mutex<Option<Option<Sender<Job>>>> = Mutex::new(None);

fn known() -> MutexGuard<'static, Option<Known>> {
    KNOWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn with_known<R>(f: impl FnOnce(&mut Known) -> R) -> R {
    f(known().get_or_insert_with(Known::default))
}

fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn start(speller: Box<dyn Speller>) -> Sender<Job> {
    let (jobs, inbox) = channel();
    std::thread::Builder::new().name("spell-check".to_owned()).spawn(move || run(speller, inbox)).expect("thread");
    jobs
}

fn run(mut speller: Box<dyn Speller>, inbox: Receiver<Job>) {
    while let Ok(job) = inbox.recv() {
        match job {
            Job::Check { paragraph, words } => {
                let misspelt = speller.misspelt(&paragraph, &words);
                with_known(|known| {
                    for word in words {
                        // Learnt or ignored while it was out: that answer stands.
                        if known.verdicts.get(&word) == Some(&Verdict::Asked) {
                            let verdict = if misspelt.contains(&word) { Verdict::Misspelt } else { Verdict::Good };
                            known.verdicts.insert(word, verdict);
                        }
                    }
                });
                GENERATION.fetch_add(1, Ordering::Relaxed);
            }
            Job::Guess(word) => {
                let guesses = speller.guesses(&word);
                with_known(|known| known.guesses.insert(word, Some(guesses)));
            }
            Job::Learn(word) => speller.learn(&word),
            Job::Ignore(word) => speller.ignore(&word),
        }
    }
}

/// The checker's inbox, starting it the first time. `None` where nothing answers.
fn with_checker<R>(f: impl FnOnce(Option<&Sender<Job>>) -> R) -> R {
    let mut checker = CHECKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(checker.get_or_insert_with(|| system_speller().map(start)).as_ref())
}

/// Sends a job. Answers whether there is a checker to send it to.
fn send(job: Job) -> bool {
    with_checker(|checker| checker.is_some_and(|jobs| jobs.send(job).is_ok()))
}

/// Puts another checker in the system's place and forgets what the last one said: a test's, which leaves the user's dictionary alone.
pub fn install(speller: Box<dyn Speller>) {
    *CHECKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Some(start(speller)));
    *known() = None;
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// What the checker would put instead of a word, once it has said. `None` while it is out asking, and always where nothing answers.
pub fn guesses(word: &str) -> Option<Vec<String>> {
    let (found, ask) = with_known(|known| match known.guesses.get(word) {
        Some(found) => (found.clone(), false),
        None => {
            known.guesses.insert(word.to_owned(), None);
            (None, true)
        }
    });
    if ask && !send(Job::Guess(word.to_owned())) {
        with_known(|known| known.guesses.remove(word));
    }
    found
}

fn accept(word: &str, job: Job) {
    with_known(|known| known.verdicts.insert(word.to_owned(), Verdict::Good));
    GENERATION.fetch_add(1, Ordering::Relaxed);
    send(job);
}

/// "Learn Spelling": the word goes in the user's dictionary, and off the screen at once.
pub fn learn(word: &str) {
    accept(word, Job::Learn(word.to_owned()));
}

/// "Ignore Spelling": the word stands for as long as the application runs.
pub fn ignore(word: &str) {
    accept(word, Job::Ignore(word.to_owned()));
}

// ---------------------------------------------------------------- words

/// Written without spaces, in another script's own way: a checker has nothing to say about these.
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

fn is_letter(c: char) -> bool {
    (c.is_alphabetic() || matches!(c as u32, 0x300..=0x36F)) && !is_cjk(c)
}

/// Whether a stretch between spaces is prose at all, and not an address, a path, a flag or a name out of some code.
fn is_prose(chunk: &str) -> bool {
    if chunk.contains("://") || chunk.contains("::") || chunk.chars().any(|c| c.is_ascii_digit() || matches!(c, '@' | '/' | '\\' | '_' | '`' | '=' | '<' | '>' | '{' | '}' | '|' | '~' | '$' | '#')) || chunk.starts_with('-') {
        return false;
    }
    // `main.rs`: a sentence's dot or colon has a space after it.
    let chars: Vec<char> = chunk.chars().collect();
    !chars.windows(3).any(|w| matches!(w[1], '.' | ':') && w[0].is_alphanumeric() && w[2].is_alphanumeric())
}

/// The words of a text worth spelling, as byte ranges.
fn words(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for chunk in text.split(char::is_whitespace).filter(|chunk| !chunk.is_empty()) {
        if !is_prose(chunk) {
            continue;
        }
        let base = chunk.as_ptr() as usize - text.as_ptr() as usize;
        let mut chars = chunk.char_indices().peekable();
        let mut open: Option<(usize, usize)> = None;
        while let Some((at, c)) = chars.next() {
            // An apostrophe belongs to a word only with a letter on each side of it.
            let inner = open.is_some() && matches!(c, '\'' | '\u{2019}') && chars.peek().is_some_and(|(_, next)| is_letter(*next));
            if is_letter(c) || inner {
                let (from, count) = open.unwrap_or((at, 0));
                open = Some((from, count + 1));
                continue;
            }
            if let Some((from, _)) = open.take().filter(|(_, count)| *count > 1) {
                out.push(base + from..base + at);
            }
        }
        if let Some((from, _)) = open.filter(|(_, count)| *count > 1) {
            out.push(base + from..base + chunk.len());
        }
    }
    out
}

// ---------------------------------------------------------------- one field's

/// One field's `spellCheck`. It works out what to draw only when the text, the caret's word or the answers have changed.
#[derive(Default)]
pub(crate) struct Spelling {
    misspelt: Vec<Range<usize>>,
    /// What `misspelt` was worked out from: the text's revision, the answers' generation, and the word being written.
    seen: Option<(u64, u64, Option<usize>)>,
    /// The revision and caret of the last frame, for telling writing from moving the caret.
    last: (u64, usize),
    /// The last thing to happen was a change to the text, so the word at the caret is still being written.
    writing: bool,
    waiting: bool,
}

impl Spelling {
    pub(crate) fn misspelt(&self) -> &[Range<usize>] {
        &self.misspelt
    }

    /// Whether an answer is still out, so another frame should look for it.
    pub(crate) fn waiting(&self) -> bool {
        self.waiting
    }

    /// `composing`: an input method holds text at the caret, which isn't text yet and leaves the word it touches unfinished.
    pub(crate) fn refresh(&mut self, text: &str, revision: u64, caret: usize, composing: bool, focused: bool, atoms: &[Range<usize>]) {
        if revision != self.last.0 {
            self.writing = true;
        } else if caret != self.last.1 {
            self.writing = false;
        }
        self.last = (revision, caret);
        let open = (composing || (focused && self.writing)).then_some(caret);
        let seen = (revision, generation(), open);
        if self.seen == Some(seen) {
            return;
        }
        self.seen = Some(seen);
        self.misspelt.clear();
        self.waiting = false;
        if !with_checker(|checker| checker.is_some()) {
            return;
        }
        let mut asks = Vec::new();
        {
            let mut guard = known();
            let known = guard.get_or_insert_with(Known::default);
            if known.verdicts.len() > KNOWN_WORDS {
                known.verdicts.clear();
            }
            let mut start = 0;
            for paragraph in text.split('\n') {
                let mut unknown = Vec::new();
                for word in words(paragraph) {
                    let spelt = &paragraph[word.clone()];
                    let word = start + word.start..start + word.end;
                    let finished = open.is_none_or(|caret| caret < word.start || word.end < caret);
                    if !finished || atoms.iter().any(|atom| atom.start < word.end && word.start < atom.end) {
                        continue;
                    }
                    match known.verdicts.get(spelt) {
                        Some(Verdict::Misspelt) => self.misspelt.push(word),
                        Some(Verdict::Good) => {}
                        Some(Verdict::Asked) => self.waiting = true,
                        None => {
                            known.verdicts.insert(spelt.to_owned(), Verdict::Asked);
                            unknown.push(spelt.to_owned());
                            self.waiting = true;
                        }
                    }
                }
                if !unknown.is_empty() {
                    asks.push(Job::Check { paragraph: paragraph.to_owned(), words: unknown });
                }
                start += paragraph.len() + 1;
            }
        }
        for ask in asks {
            send(ask);
        }
    }
}

// ---------------------------------------------------------------- the system's

#[cfg(target_os = "macos")]
fn system_speller() -> Option<Box<dyn Speller>> {
    Some(Box::new(mac::SystemSpeller::new()))
}

#[cfg(not(target_os = "macos"))]
fn system_speller() -> Option<Box<dyn Speller>> {
    None
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSArray, NSRange, NSString};

    // The class is looked up by name, so nothing else here would keep the framework linked.
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    /// `NSTextCheckingTypeOrthography`.
    const CHECK_ORTHOGRAPHY: u64 = 1 << 1;

    pub(super) struct SystemSpeller {
        /// `uniqueSpellDocumentTag`: what an ignored word is ignored in.
        tag: isize,
    }

    fn checker() -> Retained<AnyObject> {
        unsafe { msg_send![class!(NSSpellChecker), sharedSpellChecker] }
    }

    impl SystemSpeller {
        pub(super) fn new() -> Self {
            Self { tag: unsafe { msg_send![class!(NSSpellChecker), uniqueSpellDocumentTag] } }
        }

        /// The language to spell a paragraph's words in: the one it reads as, if there is a dictionary for it. Asked with no language,
        /// the checker takes a word that is good in any language the system knows, and a short or mixed paragraph reads as none, so
        /// `teh` would pass. There it falls back on the language the checker has chosen for itself.
        fn language_of(&self, paragraph: &str) -> Retained<NSString> {
            let checker = checker();
            let text = NSString::from_str(paragraph);
            let range = NSRange::new(0, text.length());
            let mut orthography: Option<Retained<AnyObject>> = None;
            let _: Retained<AnyObject> = unsafe { msg_send![&*checker, checkString: &*text, range: range, types: CHECK_ORTHOGRAPHY, options: None::<&AnyObject>, inSpellDocumentWithTag: self.tag, orthography: &mut orthography, wordCount: std::ptr::null_mut::<isize>()] };
            let available: Retained<NSArray<NSString>> = unsafe { msg_send![&*checker, availableLanguages] };
            let dictionary_for = |language: &str| available.iter().find(|name| name.to_string() == language || name.to_string().starts_with(&format!("{language}_")));
            let dominant = orthography.map(|orthography| -> Retained<NSString> { unsafe { msg_send![&*orthography, dominantLanguage] } });
            dominant.and_then(|language| dictionary_for(&language.to_string())).unwrap_or_else(|| unsafe { msg_send![&*checker, language] })
        }
    }

    impl super::Speller for SystemSpeller {
        fn misspelt(&mut self, paragraph: &str, words: &[String]) -> Vec<String> {
            autoreleasepool(|_| {
                let language = self.language_of(paragraph);
                let checker = checker();
                words
                    .iter()
                    .filter(|word| {
                        let word = NSString::from_str(word);
                        let found: NSRange = unsafe { msg_send![&*checker, checkSpellingOfString: &*word, startingAt: 0isize, language: &*language, wrap: false, inSpellDocumentWithTag: self.tag, wordCount: std::ptr::null_mut::<isize>()] };
                        found.length > 0
                    })
                    .cloned()
                    .collect()
            })
        }

        fn guesses(&mut self, word: &str) -> Vec<String> {
            autoreleasepool(|_| {
                let word = NSString::from_str(word);
                let range = NSRange::new(0, word.length());
                let found: Option<Retained<NSArray<NSString>>> = unsafe { msg_send![&*checker(), guessesForWordRange: range, inString: &*word, language: None::<&NSString>, inSpellDocumentWithTag: self.tag] };
                found.map_or_else(Vec::new, |found| found.iter().map(|guess| guess.to_string()).collect())
            })
        }

        fn learn(&mut self, word: &str) {
            autoreleasepool(|_| {
                let word = NSString::from_str(word);
                let _: () = unsafe { msg_send![&*checker(), learnWord: &*word] };
            });
        }

        fn ignore(&mut self, word: &str) {
            autoreleasepool(|_| {
                let word = NSString::from_str(word);
                let _: () = unsafe { msg_send![&*checker(), ignoreWord: &*word, inSpellDocumentWithTag: self.tag] };
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn found(text: &str) -> Vec<&str> {
        words(text).into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn spells_the_words_of_prose_and_leaves_what_is_not_prose() {
        assert_eq!(found("Teh quick, brown \"fox\" (jumped)."), ["Teh", "quick", "brown", "fox", "jumped"]);
        assert_eq!(found("don't don\u{2019}t the dogs' well-known"), ["don't", "don\u{2019}t", "the", "dogs", "well", "known"]);
        assert_eq!(found("see https://exmaple.com a@exmaple.com src/mian.rs snake_cse mian.rs std::fs `coed` --flagg h264 $HOEM"), ["see"]);
        assert_eq!(found("a I x"), Vec::<&str>::new(), "one letter is not much to spell");
        assert_eq!(found("拼写check 检查，さよなら"), ["check"], "nothing to ask about a run of ideographs");
        assert_eq!(found("cafe\u{301} na\u{ef}ve"), ["cafe\u{301}", "na\u{ef}ve"]);
    }

    #[test]
    fn a_word_keeps_its_place_in_the_text() {
        let text = "世界 teh\ncat  teh";
        assert_eq!(words(text), [7..10, 11..14, 16..19]);
    }

    /// Calls the words it was asked about out loud, so a test can tell what reached it.
    struct Fake {
        bad: Vec<&'static str>,
        asked: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl Speller for Fake {
        fn misspelt(&mut self, _paragraph: &str, words: &[String]) -> Vec<String> {
            self.asked.lock().unwrap().extend(words.iter().cloned());
            words.iter().filter(|word| self.bad.contains(&word.as_str())).cloned().collect()
        }
        fn guesses(&mut self, word: &str) -> Vec<String> {
            vec![format!("{word}-guess")]
        }
        fn learn(&mut self, _word: &str) {}
        fn ignore(&mut self, _word: &str) {}
    }

    static ONE_CHECKER: Mutex<()> = Mutex::new(());

    /// Frames until the answers are in.
    fn refreshed(spelling: &mut Spelling, text: &str, revision: u64, caret: usize, composing: bool, focused: bool) {
        let started = Instant::now();
        loop {
            spelling.refresh(text, revision, caret, composing, focused, &[]);
            if !spelling.waiting() {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "the checker never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_word_is_asked_about_once_when_it_is_finished_and_not_while_it_is_written() {
        let _one = ONE_CHECKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        install(Box::new(Fake { bad: vec!["teh"], asked: asked.clone() }));
        let mut spelling = Spelling::default();

        refreshed(&mut spelling, "teh", 1, 3, false, true);
        assert!(spelling.misspelt().is_empty(), "still being written");
        assert!(asked.lock().unwrap().is_empty(), "and nothing has asked");

        refreshed(&mut spelling, "teh ", 2, 4, false, true);
        assert_eq!(spelling.misspelt(), [0..3], "a space finishes it");

        refreshed(&mut spelling, "teh cat", 3, 7, false, true);
        assert_eq!(spelling.misspelt(), [0..3]);
        refreshed(&mut spelling, "teh cat", 3, 2, false, true);
        assert_eq!(spelling.misspelt(), [0..3], "the caret going back into a word leaves its mark alone");
        refreshed(&mut spelling, "teX cat", 4, 3, false, true);
        assert!(spelling.misspelt().is_empty(), "until it changes the word, which is being written again");

        refreshed(&mut spelling, "teh cat teh teh ", 5, 16, false, true);
        assert_eq!(spelling.misspelt(), [0..3, 8..11, 12..15]);
        assert_eq!(*asked.lock().unwrap(), ["teh", "cat"], "each word once, whatever else was typed");

        ignore("teh");
        refreshed(&mut spelling, "teh cat teh teh ", 5, 16, false, true);
        assert!(spelling.misspelt().is_empty(), "an ignored word goes at once");
    }

    #[test]
    fn what_an_input_method_is_composing_leaves_the_word_at_the_caret_unfinished_and_a_field_out_of_focus_has_none() {
        let _one = ONE_CHECKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        install(Box::new(Fake { bad: vec!["teh"], asked: Default::default() }));
        let mut spelling = Spelling::default();
        refreshed(&mut spelling, "teh", 1, 3, true, true);
        assert!(spelling.misspelt().is_empty());
        refreshed(&mut spelling, "teh", 1, 3, false, false);
        assert_eq!(spelling.misspelt(), [0..3], "focus leaving finishes the word");
    }

    #[test]
    fn an_atom_is_not_spelled() {
        let _one = ONE_CHECKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        install(Box::new(Fake { bad: vec!["teh"], asked: Default::default() }));
        let mut spelling = Spelling::default();
        let started = Instant::now();
        loop {
            spelling.refresh("teh teh ", 1, 8, false, true, &[0..4]);
            if !spelling.waiting() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(spelling.misspelt(), [4..7]);
    }

    #[test]
    fn guesses_come_back_when_they_are_ready() {
        let _one = ONE_CHECKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        install(Box::new(Fake { bad: vec![], asked: Default::default() }));
        let started = Instant::now();
        let found = loop {
            if let Some(found) = guesses("wrod") {
                break found;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(found, ["wrod-guess"]);
    }

    /// The real checker, from a thread that isn't the main one, as the app asks it. Read only: nothing here touches the dictionary.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_checker_flags_a_misspelling_and_offers_what_was_meant() {
        let mut speller = mac::SystemSpeller::new();
        let of = |words: &[&str]| words.iter().map(|word| (*word).to_owned()).collect::<Vec<_>>();
        // A sentence that reads as English, one too short to read as anything, and one mostly in another script.
        assert_eq!(speller.misspelt("This sentense has teh words.", &of(&["This", "sentense", "has", "teh", "words"])), ["sentense", "teh"]);
        assert_eq!(speller.misspelt("teh", &of(&["teh"])), ["teh"]);
        assert_eq!(speller.misspelt("我想检查 sentense 和 the 的拼写", &of(&["sentense", "the"])), ["sentense"]);
        assert!(speller.guesses("teh").iter().any(|guess| guess == "the"), "{:?}", speller.guesses("teh"));
        assert!(speller.misspelt("the", &of(&["the"])).is_empty());
    }
}
