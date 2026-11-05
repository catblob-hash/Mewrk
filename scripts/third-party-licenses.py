#!/usr/bin/env python3
"""Writes THIRD-PARTY-LICENSES.md: the crates the macOS app and its agent link, from Cargo.lock, with their license texts.

Only normal dependency edges count. Build and dev dependencies are not distributed. Proc-macros reached through normal edges are listed even
though they only run at compile time, which errs on the side of saying too much. It stops on a crate with no declared license, or a license
family the list below hasn't accepted as compatible with GPL-3.0-or-later, so a new dependency can't bring one in unnoticed.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = "aarch64-apple-darwin"
# Each set is one `cargo tree` run, as each is one `cargo build`: their features don't unify, so together they'd list crates nothing links.
ROOTS = [["mewrk", "mewrk-cef-helper"], ["mewrk-remote-agent"]]
# What an expression may offer. An OR needs only one of these; an AND needs all of them.
COMPATIBLE = {
    "0BSD", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "BSL-1.0", "CC0-1.0", "CDLA-Permissive-2.0", "IJG", "ISC", "LLVM-exception", "MIT", "MIT-0",
    "MPL-2.0", "Unicode-3.0", "Unicode-DFS-2016", "Unlicense", "Zlib",
}
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|unlicense|notice)($|[._-])", re.IGNORECASE)


def run(*args):
    return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True).stdout


def linked_crates():
    """(name, version) of every registry crate in the tree; workspace members print a path after their version."""
    found = set()
    for roots in ROOTS:
        packages = [arg for root in roots for arg in ("-p", root)]
        tree = run("cargo", "tree", *packages, "-e", "normal", "--target", TARGET, "--prefix", "none", "--format", "{p}")
        for line in tree.splitlines():
            match = re.fullmatch(r"(\S+) v(\S+)(?: \(\*\))?", line.strip())
            if match:
                found.add(match.groups())
    return found


def is_compatible(expression):
    """Whether the expression, read as SPDX with AND binding tighter than OR, leaves a way through COMPATIBLE."""
    tokens = re.findall(r"[()]|[^\s()/]+", expression.replace("/", " OR "))
    position = 0

    def alternatives():
        nonlocal position
        result = conjunction()
        while position < len(tokens) and tokens[position] == "OR":
            position += 1
            right = conjunction()
            result = result or right
        return result

    def conjunction():
        nonlocal position
        result = term()
        while position < len(tokens) and tokens[position] == "AND":
            position += 1
            right = term()
            result = result and right
        return result

    def term():
        nonlocal position
        token = tokens[position]
        position += 1
        if token == "(":
            result = alternatives()
            position += 1
            return result
        result = token.removesuffix("+") in COMPATIBLE
        if position < len(tokens) and tokens[position] == "WITH":
            result = result and tokens[position + 1] in COMPATIBLE
            position += 2
        return result

    return alternatives()


def license_texts(package):
    directory = Path(package["manifest_path"]).parent
    names = [path.name for path in directory.iterdir() if path.is_file() and LICENSE_FILE.match(path.name)]
    if package.get("license_file"):
        names.append(package["license_file"])
    texts = []
    for name in sorted(set(names)):
        path = directory / name
        if path.is_file():
            texts.append(path.read_text(errors="replace").replace("\r\n", "\n").strip())
    return texts


def main():
    wanted = linked_crates()
    metadata = json.loads(run("cargo", "metadata", "--format-version", "1", "--locked"))
    packages = [package for package in metadata["packages"] if (package["name"], package["version"]) in wanted and package["source"]]
    packages.sort(key=lambda package: (package["name"], package["version"]))
    missing = wanted - {(package["name"], package["version"]) for package in packages}
    if missing:
        sys.exit(f"not in cargo metadata: {sorted(missing)}")

    rows, by_text, without_text, refused = [], {}, [], []
    for package in packages:
        identity = f'{package["name"]} {package["version"]}'
        # Older crates write `MIT/Apache-2.0`, which means OR.
        expression = package.get("license") or ""
        if not expression or not is_compatible(expression):
            refused.append(f"{identity}: {expression or 'no declared license'}")
        rows.append(f"| {package['name']} | {package['version']} | {expression.replace('|', '/')} |")
        texts = license_texts(package)
        if not texts:
            without_text.append(identity)
        for text in texts:
            by_text.setdefault(text, []).append(identity)
    if refused:
        sys.exit("needs a decision before release:\n  " + "\n  ".join(refused))

    out = [
        "# Third-party licenses",
        "",
        "Generated by `scripts/third-party-licenses.py` from `Cargo.lock`. It lists the Rust crates the macOS app and its agent link, with the license",
        "texts those crates publish. Identical texts are listed once. Code and data that are vendored or adapted, and the Chromium Embedded Framework,",
        "are in [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md). Where a crate offers a choice of licenses, the choice is not narrowed here. Every",
        "crate below offers at least one that is compatible with Mewrk's GPL-3.0-or-later.",
        "",
        f"{len(packages)} crates, {len(by_text)} distinct license texts.",
        "",
        "## Crates",
        "",
        "| Crate | Version | License |",
        "|---|---|---|",
        *rows,
    ]
    if without_text:
        out += ["", "These crates declare a license above but publish no license file in their package:", "", *[f"- {identity}" for identity in without_text]]
    out += ["", "## License texts"]
    for text, identities in sorted(by_text.items(), key=lambda item: item[1][0]):
        out += ["", f"### {', '.join(identities)}", "", "````text", text, "````"]
    (ROOT / "THIRD-PARTY-LICENSES.md").write_text("\n".join(out) + "\n")
    print(f"THIRD-PARTY-LICENSES.md: {len(packages)} crates, {len(by_text)} texts, {len(without_text)} crates with no license file")


main()
