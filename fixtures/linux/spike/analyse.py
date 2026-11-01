#!/usr/bin/env python3
"""Read the spike's results (spike.sh OUT_DIR) and say what they show: per toolkit,
`elements` p50/p95, messages per node, the fixture's known controls listed or not —
and for each one missing, what its node in the dumped tree says about why — where
its screen extents put its nodes, and whether the reader's role words agree with
the toolkit's own role names.

Pure: it reads JSON files and prints Markdown; it opens no bus and starts nothing.

  python3 fixtures/linux/spike/analyse.py OUT_DIR > OUT_DIR/summary.md
"""

import json
import sys
from collections import Counter
from pathlib import Path

SHOWING = 25
ENABLED = 8
SENSITIVE = 24
LABELS = {"gtk3": "GTK3", "gtk4": "GTK4", "qt": "Qt", "gtk4-wayland": "GTK4"}
# The results spike.sh writes, in the order they are reported.
RESULTS = ("gtk3", "gtk4", "qt", "gtk4-wayland", "chromium")
CHROMIUM_CONTROLS = ["Chromium button", "Chromium disabled button", "Help",
                     "Chromium text field", "Chromium secure field", "Search", "Volume",
                     "Chromium checkbox", "Small", "Large", "Chromium pop-up button"]

# The reader's role words, by number (native/compux/src/linux/vocabulary.rs).
ROLE_WORDS_FILE = Path(__file__).resolve().parents[3] / "native/compux/src/linux/vocabulary.rs"


def role_words():
    text = ROLE_WORDS_FILE.read_text()
    table = text[text.index("const ROLES"):text.index("];", text.index("const ROLES"))]
    return [line.strip().strip(",").strip('"') for line in table.splitlines()[1:]
            if line.strip().startswith('"')]


def known_controls(toolkit):
    if toolkit == "chromium":
        return CHROMIUM_CONTROLS
    label = LABELS[toolkit]
    return ([f"{label} button", f"{label} checkbox", f"{label} text field",
             f"{label} secure field", f"{label} disabled button", f"{label} pop-up button",
             "Search", "Volume", "Clear All…", "Save"]
            + ["Apple", "Banana", "Cherry", "Damson", "Elderberry"]
            + ["Documents", "Pictures", "Projects", "Music", "Alpha", "Beta"]
            + ["General", "Advanced", "About", "Actions", "Entry 1", "Entry 200"])


def why_missing(node):
    """What a tree node says about why the walk did not list it."""
    reasons = []
    states = node.get("states") if isinstance(node.get("states"), list) else []
    if SHOWING not in states:
        reasons.append("not SHOWING")
    extents = node.get("extents")
    if isinstance(extents, list) and (extents[2] <= 0 or extents[3] <= 0):
        reasons.append(f"extents {extents}")
    if "extents" not in node:
        reasons.append("no Component")
    actions = node.get("actions") or []
    reasons.append(f"role {node.get('role')} {node.get('role_name')!r} actions {actions}")
    return "; ".join(reasons)


def coverage(toolkit, tree, listing):
    listed = Counter(element.get("label") for element in listing)
    named = {}
    for node in tree["nodes"]:
        named.setdefault(node.get("name"), []).append(node)
    rows = []
    for label in known_controls(toolkit):
        if listed.get(label):
            rows.append((label, f"listed x{listed[label]}", ""))
            continue
        nodes = named.get(label, [])
        if not nodes:
            rows.append((label, "MISSING", "no node with this name in the tree"))
            continue
        rows.append((label, "MISSING", " | ".join(why_missing(node) for node in nodes[:2])))
    return rows


def positions(tree):
    """How many showing nodes with a Component have their screen extents at (0, 0),
    and how many of those have a window-relative position that is not."""
    showing = [node for node in tree["nodes"] if isinstance(node.get("extents"), list)
               and SHOWING in (node.get("states") or []) and node["extents"][2] > 0]
    at_origin = [node for node in showing if node["extents"][:2] == [0, 0]]
    placed = [node for node in at_origin if isinstance(node.get("extents_window"), list)
              and node["extents_window"][:2] != [0, 0]]
    return (f"- screen extents at (0, 0): {len(at_origin)} of {len(showing)} showing nodes "
            f"with an area; {len(placed)} of those have a window-relative position")


def role_agreement(trees):
    words = role_words()
    disagree = Counter()
    for tree in trees:
        for node in tree["nodes"]:
            role, spelled = node.get("role"), node.get("role_name")
            if not isinstance(role, int) or not isinstance(spelled, str):
                continue
            ours = words[role] if role < len(words) else f"role {role}"
            if ours != spelled:
                disagree[(role, ours, spelled)] += 1
    return disagree


def tree_only_section(toolkit, tree, out):
    """A tree was dumped but `elements` gave no listing: say what it answered."""
    answer = out / f"{toolkit}-elements.err"
    said = answer.read_text().strip().splitlines()[-2:] if answer.exists() else ["no answer kept"]
    return [f"## {toolkit}", "", f"- tree: {len(tree['nodes'])} nodes", positions(tree),
            "- `elements` gave no listing: " + " / ".join(said), ""]


def toolkit_section(toolkit, out):
    tree_path, elements_path = out / f"{toolkit}-tree.json", out / f"{toolkit}-elements.json"
    if not tree_path.exists():
        return [f"## {toolkit}", "", "no results", ""], None
    tree = json.loads(tree_path.read_text())
    if not elements_path.exists():
        return tree_only_section(toolkit, tree, out), tree
    elements = json.loads(elements_path.read_text())
    runs = [run for run in elements["runs"] if run["ok"]]
    nodes = len(tree["nodes"])
    messages = elements.get("ax_messages_median") or 0
    lines = [f"## {toolkit}", "",
             f"- runs ok: {len(runs)} of {len(elements['runs'])}; p50 {elements['p50_ms']:.1f} ms, "
             f"p95 {elements['p95_ms']:.1f} ms",
             f"- tree: {nodes} nodes; `ax_messages` median {messages} "
             f"({messages / max(nodes, 1):.2f} per node of the dumped tree)",
             f"- listed: {len(elements['first'])}; truncated: "
             f"{sorted({str(run['truncated']) for run in runs})}",
             f"- note: {elements.get('note')}", positions(tree), "",
             "| known control | listed | why not |", "|---|---|---|"]
    lines += [f"| {label} | {state} | {why} |" for label, state, why in
              coverage(toolkit, tree, elements["first"])]
    roles = Counter(element.get("role") for element in elements["first"])
    lines += ["", "listed by role: " + ", ".join(f"{role} {count}" for role, count in
                                                 roles.most_common()), ""]
    return lines, tree


def main():
    out = Path(sys.argv[1])
    lines, trees = ["# M86 L0(a) spike results", ""], []
    for toolkit in RESULTS:
        section, tree = toolkit_section(toolkit, out)
        lines += section
        if tree:
            trees.append(tree)
    disagree = role_agreement(trees)
    lines += ["## role words against the toolkits' own role names", ""]
    if not disagree:
        lines.append("every node's role word matches its toolkit's GetRoleName")
    for (role, ours, spelled), count in disagree.most_common():
        lines.append(f"- role {role}: ours {ours!r}, toolkit {spelled!r} ({count} nodes)")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
