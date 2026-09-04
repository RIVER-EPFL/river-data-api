#!/usr/bin/env python3
"""Refuse an SVG that only a browser can draw.

The diagrams are read in image viewers, in editors and through GitHub's image sanitiser, none
of which apply a stylesheet or resolve CSS colour functions. Each of these has silently
produced black boxes or missing lines here before, so a generator change that reintroduces one
fails the build rather than the reader's viewer.
"""

import re
import sys

# SVG 1.1 named colours are portable; anything beyond hex and these is not.
NAMED = {"none", "white", "black", "transparent", "currentColor"}
PAINT = ("fill", "stroke", "stop-color", "flood-color")


def check(path):
    text = open(path, encoding="utf-8").read()
    problems = []
    for attribute in PAINT:
        for value in set(re.findall(rf'{attribute}="([^"]+)"', text)):
            if value in NAMED or re.fullmatch(r"#[0-9a-fA-F]{3,8}|url\(#[^)]+\)", value):
                continue
            problems.append(f'{attribute}="{value}", not a hex or named colour')
    for value in set(re.findall(r'\bstyle="([^"]*)"', text)):
        if any(f"{prop}:" in value for prop in PAINT):
            problems.append(f'paint inside style="{value}", which a sanitiser may drop')
    for value in set(re.findall(r'<(?:text|tspan)[^>]*?\b(?:x|y|dx|dy)="([^"]*(?:em|%)[^"]*)"', text)):
        problems.append(f'text coordinate "{value}" in units that are invalid there')
    if "<style" in text or "foreignObject" in text:
        problems.append("a <style> block or foreignObject, which carry browser-only rendering")
    if re.search(r'stroke-dasharray="[\s,0]*"', text):
        problems.append("an all-zero stroke-dasharray, which some renderers draw as no line")
    return problems


if __name__ == "__main__":
    failed = False
    for path in sys.argv[1:]:
        problems = check(path)
        for problem in problems:
            print(f"{path}: {problem}", file=sys.stderr)
        failed = failed or bool(problems)
    sys.exit(1 if failed else 0)
