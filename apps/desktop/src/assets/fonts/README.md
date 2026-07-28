# Bundled brand fonts

These local WOFF2 files are the exact Latin assets emitted by the current
`crow-fe` Next.js build for the font declarations in `crow-fe/app/layout.tsx`.
They are bundled because the desktop WebView has `connect-src 'none'` and must
not depend on Google Fonts or another external origin.

| File | SHA-256 |
|---|---|
| `ibm-plex-sans-latin.woff2` | `056e4e2459f57a0033c8c9c844ff19d6e42ac8602027803d4345823bcc939818` |
| `ibm-plex-mono-400-latin.woff2` | `c36f509c0a8f9f85f29cb44bc8701d8a9e0b14c499e77a884f789ead7093a7ac` |
| `ibm-plex-mono-500-latin.woff2` | `a76f53ca6612e7b3828eec2311098675b7f9849ae4169a8bcef6302aec02a6c0` |
| `ibm-plex-mono-600-latin.woff2` | `ad4580d8cb4b5f627c2d18457656732f7f7b070f7837fbc380e08054157e6f6c` |
| `tektur-latin.woff2` | `468f3e60237cb450abf4ab64f96dab0de0aee61a0339226d35899add6a1ad2ab` |

IBM Plex is distributed under the included `LICENSE-IBM-PLEX.txt`. Tektur is
distributed under the included `LICENSE-TEKTUR.txt`.
