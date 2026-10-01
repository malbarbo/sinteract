"""Derive the Sinteract fonts from the TTFs of the Liberation 2.1.5 release.

    python3 fonts/derive.py LIBERATION_DIR fonts

The derived fonts keep every character, its outline and its advance, and
the metrics that src/text.rs reads. They drop the hinting, the OpenType
layout tables and the glyph names, which no renderer of the crate reads.
The OFL reserves the name Liberation for the original fonts, so the
derived fonts take the name Sinteract. Each font also goes out as WOFF2,
for the web fonts of the HTML client.
"""

import sys
from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont

FAMILIES = {"Sans": "LiberationSans", "Serif": "LiberationSerif", "Mono": "LiberationMono"}
STYLES = {"Regular": "Regular", "Bold": "Bold", "Italic": "Italic", "BoldItalic": "Bold Italic"}
VERSION = "Version 2.1.5; sinteract 1"
MODIFIED = "Modified by the sinteract authors, with no hinting and no layout tables."
# The name IDs that stay as they are: the trademark, the manufacturer, the
# designer and their URLs, and the license notice.
KEPT_NAMES = (7, 8, 9, 11, 12, 13, 14)


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: derive.py LIBERATION_DIR TARGET_DIR")
    source, target = Path(sys.argv[1]), Path(sys.argv[2])
    for family, file in FAMILIES.items():
        for style in STYLES:
            original = source / f"{file}-{style}.ttf"
            derived = target / f"Sinteract{family}-{style}.ttf"
            derive(original, derived, family, style)
            check(original, derived)
            web = TTFont(derived, recalcTimestamp=False)
            web.flavor = "woff2"
            web.save(derived.with_suffix(".woff2"))


def derive(original: Path, derived: Path, family: str, style: str) -> None:
    # The timestamp of the source stays, so a second run writes the same bytes.
    font = TTFont(original, recalcTimestamp=False)
    options = subset.Options()
    options.hinting = False
    options.layout_features = []
    options.drop_tables += ["kern", "GPOS", "GSUB", "GDEF", "gasp", "FFTM"]
    options.glyph_names = False
    options.notdef_outline = True
    options.name_IDs = ["*"]
    options.name_languages = ["*"]
    options.prune_unicode_ranges = False
    subsetter = subset.Subsetter(options)
    subsetter.populate(unicodes=font.getBestCmap().keys())
    subsetter.subset(font)

    # The vendor of the original is Ascender, and the OFL does not let a
    # modified version carry its name.
    font["OS/2"].achVendID = "NONE"
    name = font["name"]
    copyright = name.getDebugName(0)
    postscript = f"Sinteract{family}-{style}"
    full = f"Sinteract {family}" if style == "Regular" else f"Sinteract {family} {STYLES[style]}"
    name.names = [r for r in name.names if r.platformID == 3 and r.nameID in KEPT_NAMES]
    for name_id, value in {
        0: f"{copyright}\n{MODIFIED}",
        1: f"Sinteract {family}",
        2: STYLES[style],
        3: f"{postscript} {VERSION}",
        4: full,
        5: VERSION,
        6: postscript,
    }.items():
        name.setName(value, name_id, 3, 1, 0x409)
    font.save(derived)


def check(original: Path, derived: Path) -> None:
    """Fails unless each character of `original` has the same points, contours
    and advance in `derived`, and the metrics that text.rs reads are the same."""
    a, b = TTFont(original), TTFont(derived)
    cmap_a, cmap_b = a.getBestCmap(), b.getBestCmap()
    assert cmap_a.keys() == cmap_b.keys(), f"{derived}: the characters differ"
    pairs = [(cmap_a[c], cmap_b[c]) for c in cmap_a]
    pairs.append((a.getGlyphOrder()[0], b.getGlyphOrder()[0]))
    for glyph_a, glyph_b in pairs:
        points_a = a["glyf"][glyph_a].getCoordinates(a["glyf"])
        points_b = b["glyf"][glyph_b].getCoordinates(b["glyf"])
        same = (
            list(points_a[0]) == list(points_b[0])
            and list(points_a[1]) == list(points_b[1])
            and [f & 1 for f in points_a[2]] == [f & 1 for f in points_b[2]]
            and a["hmtx"][glyph_a][0] == b["hmtx"][glyph_b][0]
        )
        assert same, f"{derived}: {glyph_a} differs"
    for table, field in [
        ("head", "unitsPerEm"),
        ("hhea", "ascent"),
        ("hhea", "descent"),
        ("OS/2", "fsSelection"),
        ("OS/2", "sTypoAscender"),
        ("OS/2", "sTypoDescender"),
        ("post", "underlinePosition"),
        ("post", "underlineThickness"),
    ]:
        assert getattr(a[table], field) == getattr(b[table], field), f"{derived}: {field}"


main()
