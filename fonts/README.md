# Fonts

Sinteract Sans, Serif and Mono derive from the Liberation fonts 2.1.5. They
keep every character, its outline and its advance, and drop the hinting, the
OpenType layout tables and the glyph names, which no renderer of the crate
reads. The fonts are under the SIL Open Font License 1.1, in `OFL.txt`. The
license reserves the name Liberation for the original fonts, so the derived
fonts take another name. `derive.py` is under the license of the crate.

The source is `liberation-fonts-ttf-2.1.5.tar.gz` from the
[2.1.5 release](https://github.com/liberationfonts/liberation-fonts/releases/tag/2.1.5),
with the SHA-256
`7191c669bf38899f73a2094ed00f7b800553364f90e2637010a69c0e268f25d0`. With
fontTools 4.57, this command writes the twelve fonts, and checks that each
character keeps its outline and its advance. It also writes each font as
WOFF2, which the HTML client in `web/` loads as a web font:

```sh
python3 fonts/derive.py liberation-fonts-ttf-2.1.5 fonts
```
