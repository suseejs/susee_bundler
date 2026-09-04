<!-- markdownlint-disable MD033 -->
<!-- markdownlint-disable MD041 -->
<div align="center">
<img src="https://susee.phothin.dev/logo/susee-bg-white.webp" width="160" height="160" alt="susee" />
  <h1>Susee Bundler</h1>
  <p></p>
</div>
<!-- markdownlint-enable MD033 -->

## Citation

This bundler module was originally written in TypeScript and has been ported
and modified to Rust by the author with assistance from the **glm-5.2:cloud**
model served via the [Ollama](https://ollama.com) platform.

Key references consulted during the port:

- Original TypeScript source — [susee v1.6.2](https://github.com/phothinmg/susee/releases/tag/1.6.2)
- [`oxc`](https://crates.io/crates/oxc) 0.144.0 — AST, parser, and codegen
  used for JavaScript/TypeScript analysis.
- [Ollama](https://ollama.com) — local model runtime hosting glm-5.2:cloud.
