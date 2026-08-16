#!/usr/bin/env python3
"""Verify the docs' references are alive: every `src/...:line` citation points
at a real file and an existing line, every backticked repo path names a file
that exists, every relative page link points at a page that exists, and every
link into this repo's own files on GitHub names a file that is actually
committed. Exit 1 with a report if anything is dead.

Scans every documentation directory the repo has — the site, `docs/kern/`,
`docs/oracle/` and `README.md` — because a citation nobody checks is a citation
that rots. Existence is the only thing it can *fail* on.

Two escapes, because some citations are *supposed* to name a file that is gone.
A page carrying `<!-- docs-check: historical -->` is skipped whole: a changelog
entry recording a deletion must cite the deleted file, and a point-in-time note
describes the tree as it was. A single line naming a deletion is excused in
place, so a present-tense page can still say what it removed.

Beyond existence there is a second, weaker question: does the cited line still
*say* the thing it was cited for? A line anchor is a bet that nothing is ever
inserted above it, and appending to a growing file loses that bet silently —
the line still exists, it just says something else now. So every anchor with a
line number also gets a content check: the words of the citing bullet against
the words of the cited line(s). Near-zero overlap is *nominated*, not failed.
Short targets legitimately score low, and a checker that cries wolf gets turned
off, so nominations are printed under their own heading and leave the exit code
alone. `--strict-anchors` makes them fatal for a CI that has decided to trust
them. A nomination a human has adjudicated is silenced in place with a trailing
`docs-check: anchor-ok` comment, in the same idiom as the historical marker."""

import re
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PAGE_DIRS = [
    ROOT / "docs" / "site" / "content",
    ROOT / "docs" / "kern",
]
REF = re.compile(r"`(src/[A-Za-z0-9_/.-]+\.rs)(?::(\d+)(?:-(\d+))?)?`")
REPO_PATH = re.compile(
    r"`((?:docs|tests|\.github|\.pi)/[A-Za-z0-9_/.-]+"
    r"\.(?:md|mdx|py|toml|yml|yaml|sh|json|txt|lock))(?::(\d+)(?:-(\d+))?)?`"
)
# A bare `FILE.md:NNN` with no directory — how ROADMAP.md indexes into FEATURES.md.
SIBLING_REF = re.compile(r"`([A-Z][A-Za-z0-9_.-]*\.mdx?):(\d+)(?:-(\d+))?`")
# Two anchor forms `REF` cannot see, because it demands a literal `src/` prefix.
# A continuation — `` `src/base/store.rs:624` ... `:636` `` — names the line and
# lets the file it belongs to stand from the citation before it, which is how the
# docs cite a run of functions out of one module without repeating the path nine
# times. A bare `place.rs:112` does the same for the file name. Together they were
# 245 of 664 line anchors: never existence-checked, never content-checked, and
# therefore free to rot exactly where re-pointing is most tedious. They resolve
# against the last file cited before them, so both are read the way a human reads
# them — and the scope resets at a heading, because a section is where a reader
# stops carrying context forward.
CONTINUATION = re.compile(r"`:(\d+)(?:-(\d+))?`")
BARE_RS = re.compile(r"`([A-Za-z0-9_]+\.rs):(\d+)(?:-(\d+))?`")
HEADING = re.compile(r"^#{1,6}\s")
# A doubly-backticked span is a *quotation* of the syntax, not a use of it: item 93
# writes `` `:533` `` to display the form it is discussing. Blanked before the two
# new forms are scanned — length-preserving, so match offsets still order correctly
# against the citations that were not blanked. Deliberately not applied to `REF`,
# `REPO_PATH` or `SIBLING_REF`: they have counted those spans since they were
# written, and two such quotations already carry an `anchor-ok` acquittal.
ILLUSTRATION = re.compile(r"``.+?``")
LINK = re.compile(r"\]\((\.\.?/[^)\s#]+\.mdx?)(?:#[^)\s]*)?\)")
# The org and the default branch both moved (yesitsfebreeze -> inner-zirkle,
# master -> main). github.com redirects a renamed org, so those links kept
# working and the rename looked free; raw.githubusercontent.com does not
# redirect, so the advertised install command 404'd instead. This pattern is
# pinned to the *current* name on purpose — an old-name URL matches nothing,
# reaches no existence check, and is exactly the silence that hid the 404.
SELF_URL = re.compile(
    r"https://(?:raw\.githubusercontent\.com/inner-zirkle/kern"
    r"|github\.com/inner-zirkle/kern/(?:blob|raw))/main/([^)\s#\"']+)"
)
# A self-link still on the old org or the old branch. Matched separately so it
# is reported as dead rather than skipped: `github.com` redirecting it today is
# a courtesy, not a guarantee, and its raw twin is already broken.
STALE_SELF_URL = re.compile(
    r"https://(?:raw\.githubusercontent\.com|github\.com)/"
    r"(?:yesitsfebreeze/kern\S*|inner-zirkle/kern/(?:blob|raw)/master\S*)"
)
HISTORICAL = "<!-- docs-check: historical -->"
GONE = re.compile(r"\b(deleted|removed|withdrawn|absorbed|superseded by)\b", re.I)

# A nomination a human has looked at and kept. Inline, on the citing line, so the
# verdict lives next to the thing it acquits.
ANCHOR_OK = "docs-check: anchor-ok"
CODE_SPAN = re.compile(r"`[^`]*`")
BULLET = re.compile(r"^\s*(?:[-*+]\s|\d+[.)]\s|#{1,6}\s|\|)")
WORD = re.compile(r"[A-Za-z0-9]+")
# `ReasonKind` has to reach a sentence that says "reason kind", or every doc line
# naming a type in prose scores zero against the line declaring it. Measured, this
# is a near-wash and not the win it looks like: against the real tree it silenced
# two false positives and one true one (`bayesian-belief.md:16`, whose target now
# matches on a stray "entity"). Kept because prose and code should tokenise alike,
# not because the numbers demanded it.
CAMEL = re.compile(r"[a-z0-9]+|[A-Z][a-z0-9]*")

# Four characters was too coarse a floor. It threw away exactly the tokens that
# distinguish a target: `acl`, `rrf`, `hub`, `run`, `dim`, `git`. Measured on the
# real tree, six of eleven false positives were nothing but a three-letter name
# the tokeniser refused to look at. Three is the floor now; two would admit `id`,
# `fn`, `to` and every other piece of syntax.
MIN_TOKEN = 3
# A stripped stem shorter than this is not a word, it is a fragment: `uses` must
# not become `us`, so the `es` rule declines and the `s` rule takes it to `use`.
STEM_MIN = 3
# Longest first, so `ies`/`ers` win over `es`/`s` and `edly` over `ed`.
SUFFIXES = ("ations", "ation", "ings", "ing", "edly", "ers", "ies", "ied", "er", "es", "ed", "ly", "s")
DOUBLE_KEEP = frozenset("lsz")


def _undouble(w: str) -> str:
    """`stemmer` → `stemm` → `stem`, `running` → `runn` → `run`. English doubles
    the final consonant before a vowel suffix; leaving the double in place is what
    kept `fn stem` from ever reaching the sentence that says "stemmer". `ll`/`ss`
    are real (`call`, `class`), so they stay."""
    if len(w) > STEM_MIN and w[-1] == w[-2] and w[-1].isalpha() and w[-1] not in DOUBLE_KEEP:
        return w[:-1]
    return w


def stem(w: str) -> str:
    """A light suffix stripper, not a linguist. Both sides of every comparison go
    through it, so consistency matters more than correctness: `stemmers`, `stemmer`
    and `stem` must agree, and it is fine that `status` lands on `statu` as long as
    it lands there from both directions."""
    for suf in SUFFIXES:
        if not w.endswith(suf) or len(w) - len(suf) < STEM_MIN:
            continue
        if suf == "s" and w.endswith("ss"):  # `class`, `less` — the s is the word
            continue
        base = w[: -len(suf)]
        if suf in ("ies", "ied"):
            base += "y"
        return _undouble(base)
    return w


# With a three-character floor the connective tissue is back in play, so the list
# has to carry it: articles, pronouns and prepositions manufacture agreement out
# of nothing. Rust's boilerplate keywords are here for the same reason — `let`,
# `pub`, `self` and `new` appear on nearly every line of the tree, so a match on
# one says only that the target is Rust. Stored stemmed, and looked up stemmed,
# because `files` and `file` must both be dropped.
STOPWORDS = frozenset(
    stem(w)
    for w in """about also because been before both cannot could does done each else even
    every from have here into itself just like made make many more most much must
    none only other over same should some still such than that their them then
    there these they this those through under until very what when where which
    while will with would your line lines file files
    and are but for its not the was who you all any can few had has her him his
    how may nor now off one our out own per see she too two use via way why yes
    did non yet had say get set new
    let mut pub ref mod self impl crate dyn
    """.split()
)
# The bar depends on what is being cited, because prose and code disagree by
# construction. Two documents describing the same thing reuse its words; two
# shared words is the bar the prototype cleared, catching eleven of eleven real
# FEATURES.md breakages. Prose citing *code* shares almost nothing on purpose —
# the sentence explains, the line implements — so measured against the real tree
# a two-word bar nominates 117 anchors, nearly all of them correct. For code the
# only believable signal is total silence: a target that shares no content word
# at all is a brace, a fragment, or drift.
PROSE_OVERLAP = 2
CODE_OVERLAP = 1
PROSE_SUFFIXES = (".md", ".mdx", ".txt")

line_counts: dict[Path, int] = {}
file_lines: dict[Path, list[str]] = {}


def lines_of(path: Path) -> int:
    if path not in line_counts:
        line_counts[path] = sum(1 for _ in path.open(encoding="utf-8", errors="replace"))
    return line_counts[path]


def text_of(path: Path) -> list[str]:
    if path not in file_lines:
        file_lines[path] = path.read_text(encoding="utf-8", errors="replace").splitlines()
    return file_lines[path]


rs_by_name: dict[str, list[Path]] | None = None


def resolve_rs(base: str, cur: Path | None) -> Path | None:
    """Which `src/` file does a bare `place.rs:112` mean? The one the surrounding
    text is already citing, if the name agrees — otherwise the only file in the
    tree with that name. `graph.rs` is three files (`base`, `gnn`, `retrieval`),
    so an ambiguous name with no antecedent resolves to nothing and is reported
    rather than guessed: a checker that picks one at random is worse than a
    checker that says it cannot tell."""
    global rs_by_name
    if cur is not None and cur.name == base:
        return cur
    if rs_by_name is None:
        idx: dict[str, list[Path]] = {}
        for p in (ROOT / "src").rglob("*.rs"):
            idx.setdefault(p.name, []).append(p)
        rs_by_name = idx
    hits = rs_by_name.get(base, [])
    return hits[0] if len(hits) == 1 else None


def tokens(text: str) -> set[str]:
    """Content words: lowercase, stemmed, three characters or more, connectives
    dropped. `merge_claims` and `mergeClaims` both reach a sentence saying "merge
    claims", because snake_case splits on the underscore and camelCase on the
    case; `fn stem` reaches "stemmer" because both stem to `stem`."""
    out: set[str] = set()
    for raw in WORD.findall(text):
        for w in CAMEL.findall(raw) + [raw]:
            w = stem(w.lower())
            if len(w) >= MIN_TOKEN and w not in STOPWORDS:
                out.add(w)
    return out


def blocks_of(lines: list[str]) -> list[tuple[int, int]]:
    """Split a page into citing contexts: a bullet, a table row, a heading, or a
    paragraph. A citation is judged against the whole block it sits in, not its
    own line, because the docs wrap prose at eighty columns and the sentence that
    explains an anchor routinely starts two lines above it."""
    spans: list[tuple[int, int]] = []
    start = None
    for i, text in enumerate(lines):
        if not text.strip():
            if start is not None:
                spans.append((start, i))
                start = None
            continue
        if start is not None and BULLET.match(text):
            spans.append((start, i))
            start = i
        elif start is None:
            start = i
    if start is not None:
        spans.append((start, len(lines)))
    return spans


def block_at(spans: list[tuple[int, int]], idx: int) -> tuple[int, int]:
    for lo, hi in spans:
        if lo <= idx < hi:
            return lo, hi
    return idx, idx + 1


def acquitted(text: str) -> bool:
    """The marker counts only outside backticks, so a page may quote it while
    explaining it — the same discipline that keeps `is_historical` honest."""
    return ANCHOR_OK in CODE_SPAN.sub("", text)


def nominate(
    context: str, citation: str, target: Path, start: int, end: int
) -> tuple[int, set[str]] | None:
    """Does the cited line still relate to the sentence citing it? Compare content
    words. Returns the shared words when they are too few to believe, else None."""
    body = text_of(target)[start - 1 : end]
    # The citation itself is metadata, not argument: `src/retrieval/walk.rs:12`
    # would otherwise match any target line containing the word "walk".
    prose = tokens(context.replace(citation, " "))
    said = tokens("\n".join(body))
    shared = prose & said
    bar = PROSE_OVERLAP if target.suffix in PROSE_SUFFIXES else CODE_OVERLAP
    return (len(shared), shared) if len(shared) < bar else None



# The direction files, singled out by name rather than swept in with a glob:
# `docs/` also holds loose research notes (`bayesian-belief.md`,
# `diskann-disk-index.md`, …) with their own long-standing, unrelated citation
# debt from older layouts — a blanket `docs/*.md` scan pulled all of it in the
# moment these files landed flat. Add a note here by name when it moves too.
FLAT_DOCS = [
    "VISION.md",
    "FEATURES.md",
    "ROADMAP.md",
    "CHANGELOG.md",
    "ORACLE.md",
]


def pages() -> list[Path]:
    found = [ROOT / "README.md"]
    found += [ROOT / "docs" / name for name in FLAT_DOCS]
    found += sorted((ROOT / "docs" / "specs").glob("*.md"))
    for d in PAGE_DIRS:
        found += sorted(d.rglob("*.md")) + sorted(d.rglob("*.mdx"))
    return found


# Standalone line only: a page that merely *describes* the marker must not exempt
# itself, which is how FEATURES.md and ROADMAP.md first went dark.
def is_historical(lines: list[str]) -> bool:
    return any(l.strip() == HISTORICAL for l in lines)


def check_page(page: Path, failures: list[str], nominations: list[str] | None = None) -> int:
    try:
        rel: Path | str = page.relative_to(ROOT)
    except ValueError:  # a selftest fixture living outside the repo
        rel = page
    lines = page.read_text(encoding="utf-8").splitlines()
    if is_historical(lines):
        return 0
    spans = blocks_of(lines)
    total = 0

    def anchor(
        citation: str, target: Path, lineno: int, label: str, start: int, end: int
    ) -> None:
        """Existence is settled by the caller; this asks whether the words agree."""
        if nominations is None:
            return
        lo, hi = block_at(spans, lineno - 1)
        # The citing line is inside its own block, so one sweep acquits both.
        if any(acquitted(l) for l in lines[lo:hi]):
            return
        if start < 1 or start > lines_of(target):
            return
        verdict = nominate("\n".join(lines[lo:hi]), citation, target, start, end)
        if verdict is not None:
            count, shared = verdict
            witness = ", ".join(sorted(shared)) if shared else "nothing"
            nominations.append(
                f"{rel}:{lineno}: {label} shares {count} word(s) with its target "
                f"({witness}) — {text_of(target)[start - 1].strip()[:60]!r}"
            )

    def check(
        citation: str, target: Path, lineno: int, name: str, cited: str, missing: str
    ) -> None:
        """Existence, then agreement. One body for all five citation forms, so a
        continuation is held to exactly what a spelled-out `src/` path is held to."""
        nonlocal total
        total += 1
        if not target.is_file():
            failures.append(f"{rel}:{lineno}: {missing}")
        elif cited and int(cited.split("-")[-1]) > lines_of(target):
            failures.append(
                f"{rel}:{lineno}: {name}:{cited} beyond EOF ({lines_of(target)} lines)"
            )
        elif cited:
            lo = int(cited.split("-")[0])
            anchor(citation, target, lineno, f"{name}:{lo}", lo, int(cited.split("-")[-1]))

    # The file a bare `:NNN` belongs to: the last one cited, reset at every heading.
    cur: Path | None = None
    # A fenced code block is prose-as-code, not a citation: a port `` `:8080` ``
    # or a `sed` address `` `graph.rs:9999` `` inside one is not a reference. The
    # fence toggles on a line starting with ``` (optionally a language tag) and
    # skips every anchor form inside it — the named blind spot from item 93's
    # third pass. No real doc carries a line-number-like token in a fence today,
    # so this is a no-op on the tree; the guard is the selftest.
    in_fence = False
    for lineno, text in enumerate(lines, 1):
        if text.startswith("```"):
            in_fence = not in_fence
            continue
        if HEADING.match(text):
            cur = None
        if GONE.search(text):
            continue
        if in_fence:
            continue
        # Offsets survive the blanking, so the sort below still reads left to right.
        # Every citation form runs over `quoted`, not raw `text`: the double-backtick
        # illustration escape (`` `src/llm.rs:11434` ``) must cover a spelled-out
        # `src/` path exactly as it covers a bare `:NNN` continuation, or an inline
        # illustrated full path reds as a phantom citation (the item 93 residual that
        # kept docs-check red from 2026-07-22 to 2026-07-24). LINK/SELF_URL below stay
        # on raw text — a page link is a real target, never illustration.
        quoted = ILLUSTRATION.sub(lambda m: " " * len(m.group(0)), text)
        found: list[tuple[int, str, re.Match[str]]] = []
        for m in REF.finditer(quoted):
            found.append((m.start(), "src", m))
        for m in REPO_PATH.finditer(quoted):
            found.append((m.start(), "repo", m))
        for m in SIBLING_REF.finditer(quoted):
            found.append((m.start(), "sibling", m))
        for m in BARE_RS.finditer(quoted):
            found.append((m.start(), "bare", m))
        for m in CONTINUATION.finditer(quoted):
            found.append((m.start(), "cont", m))
        found.sort(key=lambda e: e[0])

        # A range is carried as `lo-hi` and a single line as `lo`; `check` reads the
        # last field for the EOF test and the first for the anchor, so a range, a
        # single line and the lineless `src/crdt.rs` all go through one body.
        def span(lo: str | None, hi: str | None) -> str:
            return f"{lo}-{hi}" if hi else (lo or "")

        for _, kind, m in found:
            if kind in ("src", "repo"):
                target = ROOT / m.group(1)
                cur = target
                check(
                    m.group(0), target, lineno, m.group(1),
                    span(m.group(2), m.group(3)), f"missing file {m.group(1)}",
                )
            elif kind == "sibling":
                # Beside the citing page first, then the repo root — `FEATURES.md:159` in
                # docs/ROADMAP.md means the sibling doc. `README.md` is the one name this
                # does not hold for: every subdirectory could grow its own stub (docs/
                # already has one, a 3-line pointer at docs/index.md), so a bare
                # `README.md:NNN` citation always means the top-level README, sibling or not.
                if m.group(1) == "README.md":
                    target = ROOT / m.group(1)
                else:
                    target = page.parent / m.group(1)
                    if not target.is_file():
                        target = ROOT / m.group(1)
                cur = target
                check(
                    m.group(0), target, lineno, m.group(1),
                    span(m.group(2), m.group(3)), f"missing sibling {m.group(1)}",
                )
            elif kind == "bare":
                target_or_none = resolve_rs(m.group(1), cur)
                if target_or_none is None:
                    total += 1
                    if nominations is not None:
                        nominations.append(
                            f"{rel}:{lineno}: {m.group(0)} names no single file under "
                            f"src/ and no citation above it in this section says which"
                        )
                    continue
                cur = target_or_none
                check(
                    m.group(0), target_or_none, lineno,
                    str(target_or_none.relative_to(ROOT)),
                    span(m.group(2), m.group(3)), f"missing file {m.group(1)}",
                )
            else:  # a continuation of whatever was cited last
                if cur is None:
                    total += 1
                    if nominations is not None:
                        nominations.append(
                            f"{rel}:{lineno}: {m.group(0)} continues nothing — no file is "
                            f"cited above it in this section, so it names no line"
                        )
                    continue
                try:
                    name = str(cur.relative_to(ROOT))
                except ValueError:
                    name = cur.name
                check(
                    m.group(0), cur, lineno, name,
                    span(m.group(1), m.group(2)),
                    f"missing file {name}",
                )

        for m in LINK.finditer(text):
            total += 1
            if not (page.parent / m.group(1)).resolve().is_file():
                failures.append(f"{rel}:{lineno}: dead page link {m.group(1)}")
        for m in SELF_URL.finditer(text):
            total += 1
            if not (ROOT / m.group(1)).is_file():
                failures.append(f"{rel}:{lineno}: dead self link {m.group(1)}")
        for m in STALE_SELF_URL.finditer(text):
            total += 1
            failures.append(
                f"{rel}:{lineno}: stale self link {m.group(0)}"
                " — use inner-zirkle/kern on main"
            )
    return total


def main(strict: bool = False) -> int:
    failures: list[str] = []
    nominations: list[str] = []
    total = sum(check_page(p, failures, nominations) for p in pages())
    code = 0
    if failures:
        print(f"{len(failures)}/{total} dead references:")
        print("\n".join(failures))
        code = 1
    else:
        print(f"docs-check: {total} references exist")
    if nominations:
        print()
        print(f"nominated for review — {len(nominations)} anchor(s) whose target no longer")
        print("reads like the sentence citing it. These are SUSPICIONS, not failures:")
        print("a short or terse target legitimately shares few words. A human decides.")
        print("Keep one by appending `<!-- docs-check: anchor-ok -->` to its line.")
        print("\n".join("  " + n for n in nominations))
        if strict:
            print()
            print("--strict-anchors: failing on the nominations above.")
            code = 1
    elif code == 0:
        print("docs-check: no anchors nominated for review")
    return code


def selftest() -> None:
    assert REF.findall("see `src/base/merge.rs:20` and `src/crdt.rs`") == [
        ("src/base/merge.rs", "20", ""),
        ("src/crdt.rs", "", ""),
    ]
    assert REF.findall("`src/base/types.rs:291-296`") == [
        ("src/base/types.rs", "291", "296")
    ]
    assert REF.findall("src/base/merge.rs:20 unquoted") == []
    assert [m[0] for m in REPO_PATH.findall(
        "`docs/kern/vllm.md:17-20` and `tests/docs_check.py`"
    )] == ["docs/kern/vllm.md", "tests/docs_check.py"]
    assert SIBLING_REF.findall("see `FEATURES.md:733-736` and `ROADMAP.md:12`") == [
        ("FEATURES.md", "733", "736"),
        ("ROADMAP.md", "12", ""),
    ]
    assert SIBLING_REF.findall("`FEATURES.md`") == [], "a bare name has no line to check"
    assert REPO_PATH.findall("`docs/site/out/`") == [], "a directory is not a citation"
    assert REPO_PATH.findall("`src/base/merge.rs`") == [], "src is REF's job"
    assert LINK.findall("[a](./federation.mdx) [b](../howto/mcp.mdx#gossip)") == [
        "./federation.mdx",
        "../howto/mcp.mdx",
    ]
    assert LINK.findall("[v](../VISION.md)") == ["../VISION.md"]
    assert LINK.findall("[x](https://example.com/a.mdx)") == []
    assert SELF_URL.findall(
        "curl https://raw.githubusercontent.com/inner-zirkle/kern/main/scripts/install.sh | sh"
    ) == ["scripts/install.sh"]
    assert SELF_URL.findall(
        "[v](https://github.com/inner-zirkle/kern/blob/main/docs/VISION.md)"
    ) == ["docs/VISION.md"]
    assert SELF_URL.findall("https://github.com/inner-zirkle/kern/releases") == []
    # The two renames the checker was blind to, each pinned by a case.
    assert STALE_SELF_URL.findall(
        "https://raw.githubusercontent.com/yesitsfebreeze/kern/main/scripts/install.sh"
    ), "the old org is stale even on the new branch"
    assert STALE_SELF_URL.findall(
        "https://github.com/inner-zirkle/kern/blob/master/README.md"
    ), "the old branch is stale even under the new org"
    assert not STALE_SELF_URL.findall(
        "https://github.com/inner-zirkle/kern/blob/main/README.md"
    ), "the current form is not stale"
    assert (ROOT / "docs" / "ROADMAP.md") in pages(), "flat docs/ is scanned"
    assert (ROOT / "docs" / "specs" / "SPECIALISTS.md") in pages(), "docs/specs is scanned"
    assert (ROOT / "docs" / "kern" / "README.md") in pages(), "docs/kern is scanned"
    assert GONE.search("`docs/kern/x.md`, deleted 2026-07-20"), "a deletion excuses its line"
    assert not GONE.search("see `src/base/merge.rs:20`"), "a live citation is not excused"
    assert is_historical(["# Changelog", "", HISTORICAL, ""])
    assert not is_historical(
        ["a page holding `" + HISTORICAL + "` is skipped whole (`CHANGELOG.md`)"]
    ), "describing the marker inline must not exempt the page"
    anchor_selftest()
    print("selftest OK")


# A page that cites its sibling twice: once at a line that says what the sentence
# says, once at a line that says something else entirely — the exact shape a merge
# produces when it appends above an anchor. Written to disk rather than asserted in
# the abstract, so the fixture exercises the same code path `main` does.
FIXTURE_TARGET = """\
# Features

Retention expires a fact out of query results once its horizon passes.

The gossip transport batches deltas before it hands them to the wire.
"""
FIXTURE_PAGE = """\
# Notes

- Retention expires the fact out of query results once the horizon
  passes (`FEATURES.md:3`).
- The gossip transport batches deltas before the wire sees them
  (`FEATURES.md:5`).
- Retention expires the fact out of query results once the horizon
  passes (`FEATURES.md:5`).
- Retention expires the fact out of query results once the horizon
  passes (`FEATURES.md:5`). <!-- docs-check: anchor-ok -->
"""


def stem_selftest() -> None:
    """Each rule gets the pair it exists for: two spellings that must agree, and a
    neighbour that must NOT be dragged along with them."""
    for a, b, why in [
        ("stemmer", "stem", "the case item 93 named: `fn stem` vs the word 'stemmer'"),
        ("stemmers", "stem", "plural of the same"),
        ("running", "run", "doubled consonant before -ing"),
        ("panics", "panic", "plural verb"),
        ("expires", "expir", "-es off a verb"),
        ("evicted", "evict", "-ed off a verb"),
        ("entities", "entity", "-ies restores the y"),
        ("proportionally", "proportional", "-ly is not part of the word"),
    ]:
        assert stem(a) == stem(b), f"{a!r} must reach {b!r} — {why}: {stem(a)} != {stem(b)}"
    # The other half of every rule: words the stripper must leave alone, because a
    # stemmer that over-strips manufactures agreement instead of finding it.
    for w, why in [
        ("class", "a trailing -ss is the word, not a plural"),
        ("less", "same"),
        ("uses", "-es would leave 'us'; the -s rule takes it to 'use'"),
        ("call", "-ll survives undoubling"),
        ("need", "-ed would leave 'ne', under the floor"),
    ]:
        assert stem(w) in (w, w[:-1]), f"{w!r} over-stripped to {stem(w)!r} — {why}"
    assert stem("uses") == "use", "the -s rule catches what the -es rule declined"
    assert stem("class") == "class" and stem("less") == "less"
    # Fires: the words agree only after stemming.
    assert tokens("the stemmer is hand-rolled") & tokens("fn stem(t: &str) -> String {"), (
        "'stemmer' must reach `fn stem` — the false positive item 93 named"
    )
    # Does not fire: two unrelated words that merely share a prefix.
    assert not (tokens("the stemmer is hand-rolled") & tokens("fn stencil(t: &str) {")), (
        "a shared prefix is not a shared stem"
    )


def short_token_selftest() -> None:
    """The three-character floor, both ways."""
    assert "acl" in tokens("pub acl: Acl,"), "three letters is a word now"
    assert "rrf" in tokens("pub fn rrf(lists: &[&[EntityHit]]) {}")
    assert "hub" in tokens("pub async fn run_hub(idle_unload_secs: u64) {")
    assert tokens("carries an acl") & tokens("pub acl: Acl,"), (
        "`acl` must reach the field it names — a false positive on the real tree"
    )
    assert tokens("`run_hub` at serve.rs") & tokens("pub async fn run_hub(u64) {")
    # Two characters stays out: `id`, `fn`, `to` are syntax, not subject matter.
    assert "id" not in tokens("pub id: String,"), "two characters is still noise"
    assert "fn" not in tokens("pub fn go() {}")
    # And the floor must not let boilerplate manufacture agreement.
    assert not (tokens("a new claim arrives") & tokens("GCounter::new()")), (
        "`new` is on every Rust line and in every sentence — it proves nothing"
    )
    assert not (tokens("the daemon pins itself") & tokens("let mut cfg = self.pub_ref();")), (
        "Rust boilerplate keywords are stopwords"
    )


def anchor_selftest() -> None:
    assert tokens("Retention expires the fact") == {"retention", "expir", "fact"}, (
        "short words and stopwords drop out; what is left is stemmed"
    )
    assert tokens("merge_claims") == {"merge", "claim"}, "snake_case splits, then stems"
    assert tokens("ReasonKind") == {"reason", "kind", "reasonkind"}, "camelCase splits"
    stem_selftest()
    short_token_selftest()
    assert blocks_of(["- a", "  cont", "- b", "", "para"]) == [(0, 2), (2, 3), (4, 5)]
    assert acquitted("cited (`X.md:3`) <!-- docs-check: anchor-ok -->")
    assert not acquitted("silence one with `<!-- docs-check: anchor-ok -->`"), (
        "quoting the marker must not silence the page explaining it"
    )

    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        (d / "FEATURES.md").write_text(FIXTURE_TARGET, encoding="utf-8")
        page = d / "NOTES.md"
        page.write_text(FIXTURE_PAGE, encoding="utf-8")
        failures: list[str] = []
        nominations: list[str] = []
        check_page(page, failures, nominations)
        assert failures == [], f"every fixture line exists: {failures}"
        # Line 8 is the mismatch — retention prose pointed at the gossip line.
        assert len(nominations) == 1, f"exactly one nomination expected, got {nominations}"
        assert ":8:" in nominations[0] and "FEATURES.md:5" in nominations[0], nominations[0]
        assert "gossip transport batches" in nominations[0], (
            "the nomination must quote the line it doubts"
        )
        # Line 3 cites the line that says what it says; line 5 cites gossip prose at
        # the gossip line; line 10 is the same breakage as line 8, adjudicated.
        for good in (":3:", ":5:", ":10:"):
            assert not any(good in n for n in nominations), f"{good} must not be nominated"

        # Code targets ride a lower bar, so prove both sides of it separately.
        rs = d / "accept.rs"
        rs.write_text("pub fn accept(g: &mut GraphGnn) {\n}\n", encoding="utf-8")
        drifted = nominate("The accept path stamps the reason.", "`x`", rs, 2, 2)
        assert drifted == (0, set()), f"a closing brace shares nothing: {drifted}"
        assert nominate("The accept path stamps the reason.", "`x`", rs, 1, 1) is None, (
            "one shared word acquits a code target — prose and code agree by name only"
        )
        assert nominate("Retention expires the fact.", "`x`", rs, 1, 1) == (0, set()), (
            "an unrelated sentence over the same line is still nominated"
        )

        # The two fixes item 93 named, each proved on a real target line and each
        # given a near neighbour it must still nominate. A rule only observed
        # staying quiet has not been observed at all.
        drift = d / "drift.rs"
        drift.write_text(
            "fn stem(t: &str) -> String {\n"  # 1 — reached only by stemming
            "\tpub acl: Acl,\n"  # 2 — reached only by the 3-char floor
            "\tpub async fn run_hub(idle_unload_secs: u64) {\n"  # 3 — same
            "\tlet n = a + b;\n",  # 4 — nothing but boilerplate
            encoding="utf-8",
        )
        cases = [
            ("swap the hand-rolled stemmer for rust-stemmers", 1, None, "'stemmer' reaches `fn stem`"),
            ("swap the hand-rolled parser for a real one", 1, (0, set()), "an unrelated swap still fires"),
            ("Entity also carries an acl", 2, None, "`acl` reaches the field it names"),
            ("Entity also carries a scope list", 2, (0, set()), "a sibling word does not"),
            ("`run_hub` at serve.rs is the accept loop", 3, None, "`run`/`hub` are words now"),
            ("the reaper unloads a dead node", 3, None, "`unload` still matches on its own"),
            ("a new claim arrives", 4, (0, set()), "`let`/`new` prove only that the target is Rust"),
        ]
        for prose, line, want, why in cases:
            got = nominate(prose, "`x`", drift, line, line)
            assert got == want, f"drift.rs:{line} — {why}: wanted {want}, got {got}"

        # The two forms `REF` cannot see. Written as a page and run through
        # `check_page`, not asserted against the regexes, because the regexes are
        # the easy half: what has to hold is that a continuation is *resolved* to
        # the file above it and then held to the same existence check a spelled-out
        # path is held to. Every assertion below fails on the build that shipped
        # before this one — the past-EOF continuation went unreported there, which
        # is exactly how `ROADMAP.md:651` cited line 654 of a 146-line file.
        # The fixture names files out of the *current* tree. It used to cite
        # `src/base/types.rs` and `place.rs`, both of which the crate split
        # renamed away; the fixture then failed on its own missing files and
        # masked every assertion under it. The split also made bare names
        # nearly all unique — the crate prefix is in the filename now — so the
        # ambiguous case has to be `lib.rs`, of which there are twenty-six.
        cont = d / "CONT.md"
        cont.write_text(
            "# Section\n"
            "\n"
            "The kinds (`src/base/src/base_types.rs:10`) and a tail (`:999999`).\n"
            "A unique bare name (`ingest_place.rs:1`) needs no antecedent.\n"
            "An ambiguous one (`lib.rs:1`) has twenty-six candidates.\n"
            "Quoting the form `` `:7` `` displays it rather than citing it.\n"
            "\n"
            "# Another\n"
            "\n"
            "An orphan continuation (`:12`).\n",
            encoding="utf-8",
        )
        failures = []
        nominations = []
        total = check_page(cont, failures, nominations)
        assert len(failures) == 1 and "999999" in failures[0], (
            f"a continuation past EOF is a dead reference like any other: {failures}"
        )
        assert "src/base/src/base_types.rs" in failures[0], (
            f"and it is reported against the file it continues: {failures[0]}"
        )
        assert any("names no single file" in n for n in nominations), (
            f"`lib.rs` is twenty-six files — say so rather than pick one: {nominations}"
        )
        assert any("continues nothing" in n for n in nominations), (
            f"a heading ends the scope, so `:12` continues nothing: {nominations}"
        )
        assert total == 5, (
            "five citations: the path, its continuation, the unique bare name, the "
            f"ambiguous one and the orphan — the quoted `:7` is not one of them: {total}"
        )
        assert resolve_rs("ingest_place.rs", None) == (
            ROOT / "src" / "ingest" / "src" / "ingest_place.rs"
        ), "a bare name unique under src/ resolves with no antecedent at all"
        assert resolve_rs("lib.rs", None) is None, "an ambiguous name resolves to nothing"
        assert resolve_rs("lib.rs", ROOT / "src" / "base" / "src" / "lib.rs") == (
            ROOT / "src" / "base" / "src" / "lib.rs"
        ), "an antecedent of the same name settles it"
        assert ILLUSTRATION.sub("", "quoting `` `:7` `` here") == "quoting  here"

        # The residual item 93's third pass named: both new anchor forms are
        # matched inside fenced code blocks, where a port or a `sed` address is
        # prose-as-code, not a citation. The fence skip is the fix. A real
        # citation before the fence still resolves; the fenced `` `:8080` ``
        # (continuation past the store) and `` `graph.rs:9999` `` (bare name past
        # graph.rs) are silent with the skip and dead without it. The store moved
        # from src/base/store.rs to the store_core crate in the split.
        fence = d / "FENCE.md"
        fence.write_text(
            "# Section\n"
            "\n"
            "Real citation (`src/store_core/src/lib.rs:624`) before the fence.\n"
            "\n"
            "```rust\n"
            "listen on `:8080` and probe `graph.rs:9999` — not citations.\n"
            "```\n",
            encoding="utf-8",
        )
        failures = []
        nominations = []
        total = check_page(fence, failures, nominations)
        assert failures == [], f"fenced anchors must be skipped: {failures}"
        assert total == 1, f"only the pre-fence citation counts: {total}"

        # An inline double-backtick illustration must cover a spelled-out `src/`
        # path exactly as it covers a bare `:NNN` continuation. REF/REPO_PATH/
        # SIBLING_REF once ran on raw text while only the continuation forms ran on
        # the blanked `quoted`, so an illustrated `` `src/llm.rs:11434` `` reddened
        # as a phantom past-EOF citation — the residual that held docs-check red from
        # 2026-07-22 to 2026-07-24. All forms now run on `quoted`; the escape is
        # uniform.
        illus = d / "ILLUS.md"
        illus.write_text(
            "# Section\n"
            "\n"
            "An illustrated full path (`` `src/llm/src/llm.rs:11434` ``) is prose, not a cite.\n",
            encoding="utf-8",
        )
        failures = []
        nominations = []
        total = check_page(illus, failures, nominations)
        assert failures == [], f"an illustrated full path is silent: {failures}"
        assert total == 0, f"and counts as no citation at all: {total}"

        # Negative control: strip the double backticks and the same token is a real
        # citation past EOF — proving the silence is the escape, not a tokeniser
        # blind spot. `src/llm/src/llm.rs` is 718 lines, so :11434 is beyond EOF.
        illus.write_text(
            "# Section\n"
            "\n"
            "A bare full path (`src/llm/src/llm.rs:11434`) is a citation past EOF.\n",
            encoding="utf-8",
        )
        failures = []
        nominations = []
        check_page(illus, failures, nominations)
        assert len(failures) == 1 and "11434 beyond EOF" in failures[0], (
            f"the un-escaped token reds where the escaped one is silent: {failures}"
        )
        assert "src/llm/src/llm.rs" in failures[0], failures[0]

        line_counts.clear()
        file_lines.clear()


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        selftest()
        sys.exit(0)
    sys.exit(main(strict="--strict-anchors" in sys.argv))
