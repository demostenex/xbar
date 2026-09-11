# Vendored PulseAudio crates

`libpulse-sys` 1.23.0 and `libpulse-binding` 2.30.1 are copied from
[`jnqnfe/pulse-binding-rust`](https://github.com/jnqnfe/pulse-binding-rust) and are selected by
the root `[patch.crates-io]` section.

The packaged source provenance is retained in each crate's `.cargo_vcs_info.json`:

- `libpulse-sys`: `0ee554fe68c8fcdeb176f33bb8cd66c1cc178905`
- `libpulse-binding`: `309508b0a1de36271c6ebd8d3dbfa4b58ed3ac5e`

Local modifications are intentionally limited to the sink/source state boundary:

- `libpulse-sys/src/def.rs` represents sink and source states as transparent `c_int` newtypes,
  including the private `INIT` and `UNLINKED` constants, so an unexpected C integer remains valid
  Rust data.
- `libpulse-binding/src/def.rs` maps that open representation explicitly to known states or
  `Unknown(c_int)` without transmutes.
- `libpulse-binding/src/context/introspect.rs` tests the conversions used by sink/source info
  construction.

Upstream `LICENSE-MIT` and `LICENSE-APACHE` files are retained in both crates. Remove this vendor
patch when an audited upstream release supplies an equivalent sound boundary.
