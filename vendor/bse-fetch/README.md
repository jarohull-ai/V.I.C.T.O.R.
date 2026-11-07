# bse-fetch

[![Crates.io](https://img.shields.io/crates/v/bse-fetch?style=flat-square)](https://crates.io/crates/bse-fetch)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE)

Secure, SSRF-guarded HTTP scraper and plain-text extractor for the **BALD Search Engine**.

## Features

- **SSRF Protection**: Strict IP filtering rejecting RFC 1918, loopback, link-local, and cloud metadata endpoints.
- **Redirect Validation**: Evaluates every hop in redirect chains against SSRF rules.
- **Clean Text Extraction**: Heuristic HTML parser extracting readable body text while stripping navigation, ads, and scripts.
- **Robots.txt & Cache**: Respects crawler policies and caches responses with private file permissions (`0600`).

## License

Licensed under the MIT License. Part of the [BALD Engine](https://github.com/jarohull-ai/BALD-ENGINE).
