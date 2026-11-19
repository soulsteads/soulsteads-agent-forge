# Third-party dependency notices

Forge retains its existing Apache-2.0 license and source identity. Dependencies
retain their own licenses and copyright holders; this document does not relicense
them. The table covers the exact Cargo.lock package set, including development
and other-platform packages; not every entry is linked into a default Linux build.
The selected native runtime uses DBOS 0.6.0 with `engine`, `macros`, `runtime-tokio`
and default features disabled. SQLite is bundled via rusqlite; TLS uses rustls
and its selected crypto dependencies.

**mlua and LuaJIT are absent** from this lockfile and the selected default feature
graph. No Lua/LuaJIT code or notices are represented as shipped. Recheck if features
change. Crate archives come from crates.io and must match Cargo.lock checksums.

Actual LICENSE/COPYING/NOTICE/copyright files shipped in those crate archives are
preserved under `licenses/`, including nested native-source notices. Some crate archives omit their license text; the exact upstream license at the
commit recorded in their `.cargo_vcs_info.json` is retained instead, including
DBOS's MIT license. Origin URLs and file
hashes are in `licenses/provenance.json`. This is a bounded dependency/provenance
check, not a complete security audit or legal opinion. Binary distributors must
carry the relevant notices too.

| Locked package | Declared license | Retained notice content |
| --- | --- | --- |
| agent-client-protocol-3.3.0 | Apache-2.0 | [texts](licenses/agent-client-protocol-3.3.0/) |
| agent-client-protocol-derive-3.6.0 | Apache-2.0 | [texts](licenses/agent-client-protocol-derive-3.6.0/) |
| agent-client-protocol-schema-1.11.0 | Apache-2.0 | [texts](licenses/agent-client-protocol-schema-1.11.0/) |
| ahash-0.8.12 | MIT OR Apache-2.0 | [texts](licenses/ahash-0.8.12/) |
| aho-corasick-1.1.5 | Unlicense OR MIT | [texts](licenses/aho-corasick-1.1.5/) |
| allocator-api2-0.2.21 | MIT OR Apache-2.0 | [texts](licenses/allocator-api2-0.2.21/) |
| android_system_properties-0.1.6 | MIT OR Apache-2.0 | [texts](licenses/android_system_properties-0.1.6/) |
| anstream-1.0.0 | MIT OR Apache-2.0 | [texts](licenses/anstream-1.0.0/) |
| anstyle-1.0.14 | MIT OR Apache-2.0 | [texts](licenses/anstyle-1.0.14/) |
| anstyle-parse-1.0.0 | MIT OR Apache-2.0 | [texts](licenses/anstyle-parse-1.0.0/) |
| anstyle-query-1.1.5 | MIT OR Apache-2.0 | [texts](licenses/anstyle-query-1.1.5/) |
| anstyle-wincon-3.0.11 | MIT OR Apache-2.0 | [texts](licenses/anstyle-wincon-3.0.11/) |
| anyhow-1.0.104 | MIT OR Apache-2.0 | [texts](licenses/anyhow-1.0.104/) |
| async-trait-0.1.92 | MIT OR Apache-2.0 | [texts](licenses/async-trait-0.1.92/) |
| atoi-2.0.0 | MIT | [texts](licenses/atoi-2.0.0/) |
| atomic-waker-1.1.2 | Apache-2.0 OR MIT | [texts](licenses/atomic-waker-1.1.2/) |
| autocfg-1.5.1 | Apache-2.0 OR MIT | [texts](licenses/autocfg-1.5.1/) |
| aws-lc-rs-1.19.0 | ISC AND (Apache-2.0 OR ISC) | [texts](licenses/aws-lc-rs-1.19.0/) |
| aws-lc-sys-0.47.0 | ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0) | [texts](licenses/aws-lc-sys-0.47.0/) |
| axum-0.8.9 | MIT | [texts](licenses/axum-0.8.9/) |
| axum-core-0.5.6 | MIT | [texts](licenses/axum-core-0.5.6/) |
| base64-0.22.1 | MIT OR Apache-2.0 | [texts](licenses/base64-0.22.1/) |
| base64-0.23.1 | MIT OR Apache-2.0 | [texts](licenses/base64-0.23.1/) |
| bit-set-0.8.0 | Apache-2.0 OR MIT | [texts](licenses/bit-set-0.8.0/) |
| bit-vec-0.8.0 | Apache-2.0 OR MIT | [texts](licenses/bit-vec-0.8.0/) |
| bitflags-2.13.2 | MIT OR Apache-2.0 | [texts](licenses/bitflags-2.13.2/) |
| block-buffer-0.10.4 | MIT OR Apache-2.0 | [texts](licenses/block-buffer-0.10.4/) |
| block-buffer-0.12.1 | MIT OR Apache-2.0 | [texts](licenses/block-buffer-0.12.1/) |
| borrow-or-share-0.2.4 | MIT-0 | [texts](licenses/borrow-or-share-0.2.4/) |
| bs58-0.5.1 | MIT/Apache-2.0 | [texts](licenses/bs58-0.5.1/) |
| bumpalo-3.20.4 | MIT OR Apache-2.0 | [texts](licenses/bumpalo-3.20.4/) |
| bytecount-0.6.9 | Apache-2.0/MIT | [texts](licenses/bytecount-0.6.9/) |
| byteorder-1.5.0 | Unlicense OR MIT | [texts](licenses/byteorder-1.5.0/) |
| bytes-1.12.1 | MIT | [texts](licenses/bytes-1.12.1/) |
| cc-1.11.2 | MIT OR Apache-2.0 | [texts](licenses/cc-1.11.2/) |
| cfg-if-1.0.5 | MIT OR Apache-2.0 | [texts](licenses/cfg-if-1.0.5/) |
| cfg_aliases-0.2.2 | MIT | [texts](licenses/cfg_aliases-0.2.2/) |
| chacha20-0.10.2 | MIT OR Apache-2.0 | [texts](licenses/chacha20-0.10.2/) |
| chrono-0.4.45 | MIT OR Apache-2.0 | [texts](licenses/chrono-0.4.45/) |
| clap-4.6.7 | MIT OR Apache-2.0 | [texts](licenses/clap-4.6.7/) |
| clap_builder-4.6.7 | MIT OR Apache-2.0 | [texts](licenses/clap_builder-4.6.7/) |
| clap_derive-4.6.7 | MIT OR Apache-2.0 | [texts](licenses/clap_derive-4.6.7/) |
| clap_lex-1.1.1 | MIT OR Apache-2.0 | [texts](licenses/clap_lex-1.1.1/) |
| cmake-0.1.58 | MIT OR Apache-2.0 | [texts](licenses/cmake-0.1.58/) |
| cmov-0.5.4 | Apache-2.0 OR MIT | [texts](licenses/cmov-0.5.4/) |
| colorchoice-1.0.5 | MIT OR Apache-2.0 | [texts](licenses/colorchoice-1.0.5/) |
| combine-4.6.9 | MIT | [texts](licenses/combine-4.6.9/) |
| convert_case-0.10.0 | MIT | [texts](licenses/convert_case-0.10.0/) |
| core-foundation-0.10.1 | MIT OR Apache-2.0 | [texts](licenses/core-foundation-0.10.1/) |
| core-foundation-sys-0.8.7 | MIT OR Apache-2.0 | [texts](licenses/core-foundation-sys-0.8.7/) |
| cpufeatures-0.2.17 | MIT OR Apache-2.0 | [texts](licenses/cpufeatures-0.2.17/) |
| cpufeatures-0.3.1 | MIT OR Apache-2.0 | [texts](licenses/cpufeatures-0.3.1/) |
| crc-3.4.0 | MIT OR Apache-2.0 | [texts](licenses/crc-3.4.0/) |
| crc-catalog-2.5.0 | MIT OR Apache-2.0 | [texts](licenses/crc-catalog-2.5.0/) |
| croner-4.0.3 | MIT | [texts](licenses/croner-4.0.3/) |
| crossbeam-queue-0.3.14 | MIT OR Apache-2.0 | [texts](licenses/crossbeam-queue-0.3.14/) |
| crossbeam-utils-0.8.23 | MIT OR Apache-2.0 | [texts](licenses/crossbeam-utils-0.8.23/) |
| crypto-common-0.1.7 | MIT OR Apache-2.0 | [texts](licenses/crypto-common-0.1.7/) |
| crypto-common-0.2.2 | MIT OR Apache-2.0 | [texts](licenses/crypto-common-0.2.2/) |
| ctutils-0.4.8 | Apache-2.0 OR MIT | [texts](licenses/ctutils-0.4.8/) |
| darling-0.20.11 | MIT | [texts](licenses/darling-0.20.11/) |
| darling-0.24.1 | MIT | [texts](licenses/darling-0.24.1/) |
| darling_core-0.20.11 | MIT | [texts](licenses/darling_core-0.20.11/) |
| darling_core-0.24.1 | MIT | [texts](licenses/darling_core-0.24.1/) |
| darling_macro-0.20.11 | MIT | [texts](licenses/darling_macro-0.20.11/) |
| darling_macro-0.24.1 | MIT | [texts](licenses/darling_macro-0.24.1/) |
| data-encoding-2.11.1 | MIT | [texts](licenses/data-encoding-2.11.1/) |
| dbos-0.6.0 | MIT | [texts](licenses/dbos-0.6.0/) |
| dbos-macros-0.6.0 | MIT | [texts](licenses/dbos-macros-0.6.0/) |
| deranged-0.5.9 | MIT OR Apache-2.0 | [texts](licenses/deranged-0.5.9/) |
| derive_builder-0.20.2 | MIT OR Apache-2.0 | [texts](licenses/derive_builder-0.20.2/) |
| derive_builder_core-0.20.2 | MIT OR Apache-2.0 | [texts](licenses/derive_builder_core-0.20.2/) |
| derive_builder_macro-0.20.2 | MIT OR Apache-2.0 | [texts](licenses/derive_builder_macro-0.20.2/) |
| derive_more-2.1.1 | MIT | [texts](licenses/derive_more-2.1.1/) |
| derive_more-impl-2.1.1 | MIT | [texts](licenses/derive_more-impl-2.1.1/) |
| digest-0.10.7 | MIT OR Apache-2.0 | [texts](licenses/digest-0.10.7/) |
| digest-0.11.3 | MIT OR Apache-2.0 | [texts](licenses/digest-0.11.3/) |
| displaydoc-0.2.7 | MIT OR Apache-2.0 | [texts](licenses/displaydoc-0.2.7/) |
| dotenvy-0.15.7 | MIT | [texts](licenses/dotenvy-0.15.7/) |
| dunce-1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 | [texts](licenses/dunce-1.0.5/) |
| dyn-clone-1.0.20 | MIT OR Apache-2.0 | [texts](licenses/dyn-clone-1.0.20/) |
| either-1.19.0 | MIT OR Apache-2.0 | [texts](licenses/either-1.19.0/) |
| email_address-0.2.9 | MIT | [texts](licenses/email_address-0.2.9/) |
| equivalent-1.0.3 | Apache-2.0 OR MIT | [texts](licenses/equivalent-1.0.3/) |
| erased-serde-0.4.11 | MIT OR Apache-2.0 | [texts](licenses/erased-serde-0.4.11/) |
| errno-0.3.15 | MIT OR Apache-2.0 | [texts](licenses/errno-0.3.15/) |
| etcetera-0.11.0 | MIT OR Apache-2.0 | [texts](licenses/etcetera-0.11.0/) |
| event-listener-5.4.2 | Apache-2.0 OR MIT | [texts](licenses/event-listener-5.4.2/) |
| fallible-iterator-0.3.0 | MIT/Apache-2.0 | [texts](licenses/fallible-iterator-0.3.0/) |
| fallible-streaming-iterator-0.1.9 | MIT/Apache-2.0 | [texts](licenses/fallible-streaming-iterator-0.1.9/) |
| fancy-regex-0.20.2 | MIT | [texts](licenses/fancy-regex-0.20.2/) |
| fastrand-2.5.0 | Apache-2.0 OR MIT | [texts](licenses/fastrand-2.5.0/) |
| find-msvc-tools-0.1.15 | MIT OR Apache-2.0 | [texts](licenses/find-msvc-tools-0.1.15/) |
| fixedbitset-0.5.9 | MIT OR Apache-2.0 | [texts](licenses/fixedbitset-0.5.9/) |
| fluent-uri-0.4.1 | MIT | [texts](licenses/fluent-uri-0.4.1/) |
| fnv-1.0.7 | Apache-2.0 / MIT | [texts](licenses/fnv-1.0.7/) |
| foldhash-0.2.0 | Zlib | [texts](licenses/foldhash-0.2.0/) |
| form_urlencoded-1.2.2 | MIT OR Apache-2.0 | [texts](licenses/form_urlencoded-1.2.2/) |
| fraction-0.17.0 | MIT OR Apache-2.0 | [texts](licenses/fraction-0.17.0/) |
| fs_extra-1.3.0 | MIT | [texts](licenses/fs_extra-1.3.0/) |
| futures-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-0.3.34/) |
| futures-channel-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-channel-0.3.34/) |
| futures-concurrency-7.7.1 | MIT OR Apache-2.0 | [texts](licenses/futures-concurrency-7.7.1/) |
| futures-core-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-core-0.3.34/) |
| futures-executor-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-executor-0.3.34/) |
| futures-intrusive-0.5.0 | MIT OR Apache-2.0 | [texts](licenses/futures-intrusive-0.5.0/) |
| futures-io-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-io-0.3.34/) |
| futures-lite-2.6.1 | Apache-2.0 OR MIT | [texts](licenses/futures-lite-2.6.1/) |
| futures-macro-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-macro-0.3.34/) |
| futures-sink-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-sink-0.3.34/) |
| futures-task-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-task-0.3.34/) |
| futures-util-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/futures-util-0.3.34/) |
| generic-array-0.14.7 | MIT | [texts](licenses/generic-array-0.14.7/) |
| getrandom-0.2.10 | MIT OR Apache-2.0 | [texts](licenses/getrandom-0.2.10/) |
| getrandom-0.3.4 | MIT OR Apache-2.0 | [texts](licenses/getrandom-0.3.4/) |
| getrandom-0.4.3 | MIT OR Apache-2.0 | [texts](licenses/getrandom-0.4.3/) |
| hashbrown-0.12.3 | MIT OR Apache-2.0 | [texts](licenses/hashbrown-0.12.3/) |
| hashbrown-0.14.5 | MIT OR Apache-2.0 | [texts](licenses/hashbrown-0.14.5/) |
| hashbrown-0.16.1 | MIT OR Apache-2.0 | [texts](licenses/hashbrown-0.16.1/) |
| hashbrown-0.17.1 | MIT OR Apache-2.0 | [texts](licenses/hashbrown-0.17.1/) |
| hashlink-0.11.1 | MIT OR Apache-2.0 | [texts](licenses/hashlink-0.11.1/) |
| hashlink-0.8.4 | MIT OR Apache-2.0 | [texts](licenses/hashlink-0.8.4/) |
| heck-0.5.0 | MIT OR Apache-2.0 | [texts](licenses/heck-0.5.0/) |
| hex-0.4.3 | MIT OR Apache-2.0 | [texts](licenses/hex-0.4.3/) |
| hkdf-0.13.0 | MIT OR Apache-2.0 | [texts](licenses/hkdf-0.13.0/) |
| hmac-0.13.0 | MIT OR Apache-2.0 | [texts](licenses/hmac-0.13.0/) |
| http-1.5.0 | MIT OR Apache-2.0 | [texts](licenses/http-1.5.0/) |
| http-body-1.1.0 | MIT | [texts](licenses/http-body-1.1.0/) |
| http-body-util-0.1.5 | MIT | [texts](licenses/http-body-util-0.1.5/) |
| httparse-1.10.1 | MIT OR Apache-2.0 | [texts](licenses/httparse-1.10.1/) |
| httpdate-1.0.3 | MIT OR Apache-2.0 | [texts](licenses/httpdate-1.0.3/) |
| hybrid-array-0.4.15 | MIT OR Apache-2.0 | [texts](licenses/hybrid-array-0.4.15/) |
| hyper-1.12.0 | MIT | [texts](licenses/hyper-1.12.0/) |
| hyper-rustls-0.27.10 | Apache-2.0 OR ISC OR MIT | [texts](licenses/hyper-rustls-0.27.10/) |
| hyper-util-0.1.21 | MIT | [texts](licenses/hyper-util-0.1.21/) |
| iana-time-zone-0.1.65 | MIT OR Apache-2.0 | [texts](licenses/iana-time-zone-0.1.65/) |
| iana-time-zone-haiku-0.1.2 | MIT OR Apache-2.0 | [texts](licenses/iana-time-zone-haiku-0.1.2/) |
| icu_collections-2.4.0 | Unicode-3.0 | [texts](licenses/icu_collections-2.4.0/) |
| icu_locale_core-2.4.0 | Unicode-3.0 | [texts](licenses/icu_locale_core-2.4.0/) |
| icu_normalizer-2.4.0 | Unicode-3.0 | [texts](licenses/icu_normalizer-2.4.0/) |
| icu_normalizer_data-2.4.0 | Unicode-3.0 | [texts](licenses/icu_normalizer_data-2.4.0/) |
| icu_properties-2.4.0 | Unicode-3.0 | [texts](licenses/icu_properties-2.4.0/) |
| icu_properties_data-2.4.0 | Unicode-3.0 | [texts](licenses/icu_properties_data-2.4.0/) |
| icu_provider-2.4.0 | Unicode-3.0 | [texts](licenses/icu_provider-2.4.0/) |
| ident_case-1.0.1 | MIT/Apache-2.0 | [texts](licenses/ident_case-1.0.1/) |
| idna-1.1.0 | MIT OR Apache-2.0 | [texts](licenses/idna-1.1.0/) |
| idna_adapter-1.2.2 | Apache-2.0 OR MIT | [texts](licenses/idna_adapter-1.2.2/) |
| indexmap-1.9.3 | Apache-2.0 OR MIT | [texts](licenses/indexmap-1.9.3/) |
| indexmap-2.14.2 | Apache-2.0 OR MIT | [texts](licenses/indexmap-2.14.2/) |
| ipnet-2.12.2 | MIT OR Apache-2.0 | [texts](licenses/ipnet-2.12.2/) |
| is_terminal_polyfill-1.70.2 | MIT OR Apache-2.0 | [texts](licenses/is_terminal_polyfill-1.70.2/) |
| itoa-1.0.19 | MIT OR Apache-2.0 | [texts](licenses/itoa-1.0.19/) |
| jiff-0.2.43 | Unlicense OR MIT | [texts](licenses/jiff-0.2.43/) |
| jiff-core-0.1.5 | Unlicense OR MIT | [texts](licenses/jiff-core-0.1.5/) |
| jiff-static-0.2.43 | Unlicense OR MIT | [texts](licenses/jiff-static-0.2.43/) |
| jiff-tzdb-0.1.9 | Unlicense OR MIT | [texts](licenses/jiff-tzdb-0.1.9/) |
| jiff-tzdb-platform-0.1.3 | Unlicense OR MIT | [texts](licenses/jiff-tzdb-platform-0.1.3/) |
| jni-0.22.4 | MIT OR Apache-2.0 | [texts](licenses/jni-0.22.4/) |
| jni-macros-0.22.4 | MIT OR Apache-2.0 | [texts](licenses/jni-macros-0.22.4/) |
| jni-sys-0.4.1 | MIT OR Apache-2.0 | [texts](licenses/jni-sys-0.4.1/) |
| jni-sys-macros-0.4.1 | MIT OR Apache-2.0 | [texts](licenses/jni-sys-macros-0.4.1/) |
| jobserver-0.1.35 | MIT OR Apache-2.0 | [texts](licenses/jobserver-0.1.35/) |
| js-sys-0.3.107 | MIT OR Apache-2.0 | [texts](licenses/js-sys-0.3.107/) |
| jsonschema-0.61.1 | MIT | [texts](licenses/jsonschema-0.61.1/) |
| jsonschema-regex-0.61.2 | MIT | [texts](licenses/jsonschema-regex-0.61.2/) |
| jsonschema-value-0.61.2 | MIT | [texts](licenses/jsonschema-value-0.61.2/) |
| libc-0.2.191 | MIT OR Apache-2.0 | [texts](licenses/libc-0.2.191/) |
| libsqlite3-sys-0.27.0 | MIT | [texts](licenses/libsqlite3-sys-0.27.0/) |
| linux-raw-sys-0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | [texts](licenses/linux-raw-sys-0.12.1/) |
| litemap-0.8.3 | Unicode-3.0 | [texts](licenses/litemap-0.8.3/) |
| lock_api-0.4.14 | MIT OR Apache-2.0 | [texts](licenses/lock_api-0.4.14/) |
| log-0.4.35 | MIT OR Apache-2.0 | [texts](licenses/log-0.4.35/) |
| lru-slab-0.1.3 | MIT OR Apache-2.0 OR Zlib | [texts](licenses/lru-slab-0.1.3/) |
| matchit-0.8.4 | MIT AND BSD-3-Clause | [texts](licenses/matchit-0.8.4/) |
| md-5-0.11.0 | MIT OR Apache-2.0 | [texts](licenses/md-5-0.11.0/) |
| memchr-2.8.3 | Unlicense OR MIT | [texts](licenses/memchr-2.8.3/) |
| micromap-0.3.0 | MIT | [texts](licenses/micromap-0.3.0/) |
| mime-0.3.18 | MIT OR Apache-2.0 | [texts](licenses/mime-0.3.18/) |
| mio-1.2.5 | MIT | [texts](licenses/mio-1.2.5/) |
| num-0.4.3 | MIT OR Apache-2.0 | [texts](licenses/num-0.4.3/) |
| num-bigint-0.4.8 | MIT OR Apache-2.0 | [texts](licenses/num-bigint-0.4.8/) |
| num-cmp-0.1.0 | MIT/Apache-2.0 | [texts](licenses/num-cmp-0.1.0/) |
| num-complex-0.4.6 | MIT OR Apache-2.0 | [texts](licenses/num-complex-0.4.6/) |
| num-conv-0.2.2 | MIT OR Apache-2.0 | [texts](licenses/num-conv-0.2.2/) |
| num-integer-0.1.47 | MIT OR Apache-2.0 | [texts](licenses/num-integer-0.1.47/) |
| num-iter-0.1.46 | MIT OR Apache-2.0 | [texts](licenses/num-iter-0.1.46/) |
| num-rational-0.4.2 | MIT OR Apache-2.0 | [texts](licenses/num-rational-0.4.2/) |
| num-traits-0.2.19 | MIT OR Apache-2.0 | [texts](licenses/num-traits-0.2.19/) |
| once_cell-1.21.4 | MIT OR Apache-2.0 | [texts](licenses/once_cell-1.21.4/) |
| once_cell_polyfill-1.70.2 | MIT OR Apache-2.0 | [texts](licenses/once_cell_polyfill-1.70.2/) |
| openssl-probe-0.2.2 | MIT OR Apache-2.0 | [texts](licenses/openssl-probe-0.2.2/) |
| outref-0.5.2 | MIT | [texts](licenses/outref-0.5.2/) |
| parking-2.2.1 | Apache-2.0 OR MIT | [texts](licenses/parking-2.2.1/) |
| parking_lot-0.12.5 | MIT OR Apache-2.0 | [texts](licenses/parking_lot-0.12.5/) |
| parking_lot_core-0.9.12 | MIT OR Apache-2.0 | [texts](licenses/parking_lot_core-0.9.12/) |
| pastey-0.2.3 | MIT OR Apache-2.0 | [texts](licenses/pastey-0.2.3/) |
| percent-encoding-2.3.2 | MIT OR Apache-2.0 | [texts](licenses/percent-encoding-2.3.2/) |
| pin-project-1.1.13 | Apache-2.0 OR MIT | [texts](licenses/pin-project-1.1.13/) |
| pin-project-internal-1.1.13 | Apache-2.0 OR MIT | [texts](licenses/pin-project-internal-1.1.13/) |
| pin-project-lite-0.2.17 | Apache-2.0 OR MIT | [texts](licenses/pin-project-lite-0.2.17/) |
| pkg-config-0.3.34 | MIT OR Apache-2.0 | [texts](licenses/pkg-config-0.3.34/) |
| portable-atomic-1.15.0 | Apache-2.0 OR MIT | [texts](licenses/portable-atomic-1.15.0/) |
| portable-atomic-util-0.2.8 | Apache-2.0 OR MIT | [texts](licenses/portable-atomic-util-0.2.8/) |
| potential_utf-0.1.6 | Unicode-3.0 | [texts](licenses/potential_utf-0.1.6/) |
| powerfmt-0.2.1 | MIT OR Apache-2.0 | [texts](licenses/powerfmt-0.2.1/) |
| proc-macro2-1.0.107 | MIT OR Apache-2.0 | [texts](licenses/proc-macro2-1.0.107/) |
| quinn-0.11.12 | MIT OR Apache-2.0 | [texts](licenses/quinn-0.11.12/) |
| quinn-proto-0.11.19 | MIT OR Apache-2.0 | [texts](licenses/quinn-proto-0.11.19/) |
| quinn-udp-0.5.16 | MIT OR Apache-2.0 | [texts](licenses/quinn-udp-0.5.16/) |
| quote-1.0.47 | MIT OR Apache-2.0 | [texts](licenses/quote-1.0.47/) |
| r-efi-5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | [texts](licenses/r-efi-5.3.0/) |
| r-efi-6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | [texts](licenses/r-efi-6.0.0/) |
| rand-0.10.3 | MIT OR Apache-2.0 | [texts](licenses/rand-0.10.3/) |
| rand_core-0.10.1 | MIT OR Apache-2.0 | [texts](licenses/rand_core-0.10.1/) |
| rand_pcg-0.10.2 | MIT OR Apache-2.0 | [texts](licenses/rand_pcg-0.10.2/) |
| redox_syscall-0.5.18 | MIT | [texts](licenses/redox_syscall-0.5.18/) |
| ref-cast-1.0.27 | MIT OR Apache-2.0 | [texts](licenses/ref-cast-1.0.27/) |
| ref-cast-impl-1.0.27 | MIT OR Apache-2.0 | [texts](licenses/ref-cast-impl-1.0.27/) |
| referencing-0.61.2 | MIT | [texts](licenses/referencing-0.61.2/) |
| regex-1.13.1 | MIT OR Apache-2.0 | [texts](licenses/regex-1.13.1/) |
| regex-automata-0.4.18 | MIT OR Apache-2.0 | [texts](licenses/regex-automata-0.4.18/) |
| regex-syntax-0.8.11 | MIT OR Apache-2.0 | [texts](licenses/regex-syntax-0.8.11/) |
| reqwest-0.13.5 | MIT OR Apache-2.0 | [texts](licenses/reqwest-0.13.5/) |
| ring-0.17.14 | Apache-2.0 AND ISC | [texts](licenses/ring-0.17.14/) |
| rmcp-3.6.0 | Apache-2.0 | [texts](licenses/rmcp-3.6.0/) |
| rmcp-macros-3.10.0 | Apache-2.0 | [texts](licenses/rmcp-macros-3.10.0/) |
| rusqlite-0.30.0 | MIT | [texts](licenses/rusqlite-0.30.0/) |
| rustc-hash-2.1.3 | Apache-2.0 OR MIT | [texts](licenses/rustc-hash-2.1.3/) |
| rustc_version-0.4.1 | MIT OR Apache-2.0 | [texts](licenses/rustc_version-0.4.1/) |
| rustix-1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | [texts](licenses/rustix-1.1.6/) |
| rustls-0.23.46 | Apache-2.0 OR ISC OR MIT | [texts](licenses/rustls-0.23.46/) |
| rustls-native-certs-0.8.5 | Apache-2.0 OR ISC OR MIT | [texts](licenses/rustls-native-certs-0.8.5/) |
| rustls-pki-types-1.16.0 | MIT OR Apache-2.0 | [texts](licenses/rustls-pki-types-1.16.0/) |
| rustls-platform-verifier-0.7.1 | MIT OR Apache-2.0 | [texts](licenses/rustls-platform-verifier-0.7.1/) |
| rustls-platform-verifier-android-0.2.0 | MIT OR Apache-2.0 | [texts](licenses/rustls-platform-verifier-android-0.2.0/) |
| rustls-webpki-0.103.17 | ISC | [texts](licenses/rustls-webpki-0.103.17/) |
| rustversion-1.0.23 | MIT OR Apache-2.0 | [texts](licenses/rustversion-1.0.23/) |
| ryu-1.0.23 | Apache-2.0 OR BSL-1.0 | [texts](licenses/ryu-1.0.23/) |
| same-file-1.0.6 | Unlicense/MIT | [texts](licenses/same-file-1.0.6/) |
| schannel-0.1.30 | MIT | [texts](licenses/schannel-0.1.30/) |
| schemars-0.9.0 | MIT | [texts](licenses/schemars-0.9.0/) |
| schemars-1.2.3 | MIT | [texts](licenses/schemars-1.2.3/) |
| schemars_derive-1.2.3 | MIT | [texts](licenses/schemars_derive-1.2.3/) |
| scopeguard-1.2.0 | MIT OR Apache-2.0 | [texts](licenses/scopeguard-1.2.0/) |
| security-framework-3.7.0 | MIT OR Apache-2.0 | [texts](licenses/security-framework-3.7.0/) |
| security-framework-sys-2.17.0 | MIT OR Apache-2.0 | [texts](licenses/security-framework-sys-2.17.0/) |
| semver-1.0.28 | MIT OR Apache-2.0 | [texts](licenses/semver-1.0.28/) |
| serde-1.0.229 | MIT OR Apache-2.0 | [texts](licenses/serde-1.0.229/) |
| serde_core-1.0.229 | MIT OR Apache-2.0 | [texts](licenses/serde_core-1.0.229/) |
| serde_derive-1.0.229 | MIT OR Apache-2.0 | [texts](licenses/serde_derive-1.0.229/) |
| serde_derive_internals-0.30.0 | MIT OR Apache-2.0 | [texts](licenses/serde_derive_internals-0.30.0/) |
| serde_json-1.0.154 | MIT OR Apache-2.0 | [texts](licenses/serde_json-1.0.154/) |
| serde_path_to_error-0.1.20 | MIT OR Apache-2.0 | [texts](licenses/serde_path_to_error-0.1.20/) |
| serde_urlencoded-0.7.1 | MIT/Apache-2.0 | [texts](licenses/serde_urlencoded-0.7.1/) |
| serde_with-3.24.0 | MIT OR Apache-2.0 | [texts](licenses/serde_with-3.24.0/) |
| serde_with_macros-3.24.0 | MIT OR Apache-2.0 | [texts](licenses/serde_with_macros-3.24.0/) |
| sha2-0.10.8 | MIT OR Apache-2.0 | [texts](licenses/sha2-0.10.8/) |
| sha2-0.11.1 | MIT OR Apache-2.0 | [texts](licenses/sha2-0.11.1/) |
| shlex-2.0.1 | MIT OR Apache-2.0 | [texts](licenses/shlex-2.0.1/) |
| signal-hook-registry-1.4.8 | MIT OR Apache-2.0 | [texts](licenses/signal-hook-registry-1.4.8/) |
| simd_cesu8-1.2.0 | Apache-2.0 OR MIT | [texts](licenses/simd_cesu8-1.2.0/) |
| simdutf8-0.1.5 | MIT OR Apache-2.0 | [texts](licenses/simdutf8-0.1.5/) |
| slab-0.4.12 | MIT | [texts](licenses/slab-0.4.12/) |
| smallvec-1.16.5 | MIT OR Apache-2.0 | [texts](licenses/smallvec-1.16.5/) |
| socket2-0.6.5 | MIT OR Apache-2.0 | [texts](licenses/socket2-0.6.5/) |
| sqlx-0.9.0 | MIT OR Apache-2.0 | [texts](licenses/sqlx-0.9.0/) |
| sqlx-core-0.9.0 | MIT OR Apache-2.0 | [texts](licenses/sqlx-core-0.9.0/) |
| sqlx-macros-0.9.0 | MIT OR Apache-2.0 | [texts](licenses/sqlx-macros-0.9.0/) |
| sqlx-macros-core-0.9.0 | MIT OR Apache-2.0 | [texts](licenses/sqlx-macros-core-0.9.0/) |
| sqlx-postgres-0.9.0 | MIT OR Apache-2.0 | [texts](licenses/sqlx-postgres-0.9.0/) |
| stable_deref_trait-1.2.1 | MIT OR Apache-2.0 | [texts](licenses/stable_deref_trait-1.2.1/) |
| stringprep-0.1.5 | MIT/Apache-2.0 | [texts](licenses/stringprep-0.1.5/) |
| strsim-0.11.1 | MIT | [texts](licenses/strsim-0.11.1/) |
| strum-0.27.2 | MIT | [texts](licenses/strum-0.27.2/) |
| strum-0.28.0 | MIT | [texts](licenses/strum-0.28.0/) |
| strum_macros-0.27.2 | MIT | [texts](licenses/strum_macros-0.27.2/) |
| strum_macros-0.28.0 | MIT | [texts](licenses/strum_macros-0.28.0/) |
| subtle-2.6.1 | BSD-3-Clause | [texts](licenses/subtle-2.6.1/) |
| syn-2.0.119 | MIT OR Apache-2.0 | [texts](licenses/syn-2.0.119/) |
| syn-3.0.10 | MIT OR Apache-2.0 | [texts](licenses/syn-3.0.10/) |
| sync_wrapper-1.0.2 | Apache-2.0 | [texts](licenses/sync_wrapper-1.0.2/) |
| synstructure-0.14.0 | MIT | [texts](licenses/synstructure-0.14.0/) |
| tempfile-3.27.0 | MIT OR Apache-2.0 | [texts](licenses/tempfile-3.27.0/) |
| thiserror-2.0.21 | MIT OR Apache-2.0 | [texts](licenses/thiserror-2.0.21/) |
| thiserror-impl-2.0.21 | MIT OR Apache-2.0 | [texts](licenses/thiserror-impl-2.0.21/) |
| time-0.3.55 | MIT OR Apache-2.0 | [texts](licenses/time-0.3.55/) |
| time-core-0.1.9 | MIT OR Apache-2.0 | [texts](licenses/time-core-0.1.9/) |
| time-macros-0.2.32 | MIT OR Apache-2.0 | [texts](licenses/time-macros-0.2.32/) |
| tinystr-0.8.5 | Unicode-3.0 | [texts](licenses/tinystr-0.8.5/) |
| tinyvec-1.13.3 | Zlib OR Apache-2.0 OR MIT | [texts](licenses/tinyvec-1.13.3/) |
| tokio-1.50.0 | MIT | [texts](licenses/tokio-1.50.0/) |
| tokio-macros-2.6.1 | MIT | [texts](licenses/tokio-macros-2.6.1/) |
| tokio-rustls-0.26.6 | MIT OR Apache-2.0 | [texts](licenses/tokio-rustls-0.26.6/) |
| tokio-stream-0.1.19 | MIT | [texts](licenses/tokio-stream-0.1.19/) |
| tokio-util-0.7.19 | MIT | [texts](licenses/tokio-util-0.7.19/) |
| tower-0.5.3 | MIT | [texts](licenses/tower-0.5.3/) |
| tower-http-0.6.11 | MIT | [texts](licenses/tower-http-0.6.11/) |
| tower-layer-0.3.3 | MIT | [texts](licenses/tower-layer-0.3.3/) |
| tower-service-0.3.3 | MIT | [texts](licenses/tower-service-0.3.3/) |
| tracing-0.1.44 | MIT | [texts](licenses/tracing-0.1.44/) |
| tracing-attributes-0.1.31 | MIT | [texts](licenses/tracing-attributes-0.1.31/) |
| tracing-core-0.1.36 | MIT | [texts](licenses/tracing-core-0.1.36/) |
| try-lock-0.2.5 | MIT | [texts](licenses/try-lock-0.2.5/) |
| typeid-1.0.3 | MIT OR Apache-2.0 | [texts](licenses/typeid-1.0.3/) |
| typenum-1.20.1 | MIT OR Apache-2.0 | [texts](licenses/typenum-1.20.1/) |
| unicode-bidi-0.3.18 | MIT OR Apache-2.0 | [texts](licenses/unicode-bidi-0.3.18/) |
| unicode-general-category-1.1.0 | Apache-2.0 | [texts](licenses/unicode-general-category-1.1.0/) |
| unicode-ident-1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | [texts](licenses/unicode-ident-1.0.27/) |
| unicode-normalization-0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | [texts](licenses/unicode-normalization-0.1.26/) |
| unicode-properties-0.1.5 | (MIT OR Apache-2.0) AND Unicode-3.0 | [texts](licenses/unicode-properties-0.1.5/) |
| unicode-segmentation-1.13.3 | MIT OR Apache-2.0 | [texts](licenses/unicode-segmentation-1.13.3/) |
| unicode-xid-0.2.6 | MIT OR Apache-2.0 | [texts](licenses/unicode-xid-0.2.6/) |
| untrusted-0.9.0 | ISC | [texts](licenses/untrusted-0.9.0/) |
| url-2.5.8 | MIT OR Apache-2.0 | [texts](licenses/url-2.5.8/) |
| utf8_iter-1.1.0 | Apache-2.0 OR MIT | [texts](licenses/utf8_iter-1.1.0/) |
| utf8parse-0.2.2 | Apache-2.0 OR MIT | [texts](licenses/utf8parse-0.2.2/) |
| utf_types-1.0.0 | Apache-2.0 OR MIT | [texts](licenses/utf_types-1.0.0/) |
| uuid-1.29.2 | Apache-2.0 OR MIT | [texts](licenses/uuid-1.29.2/) |
| uuid-simd-0.8.0 | MIT | [texts](licenses/uuid-simd-0.8.0/) |
| vcpkg-0.2.15 | MIT/Apache-2.0 | [texts](licenses/vcpkg-0.2.15/) |
| version_check-0.9.5 | MIT/Apache-2.0 | [texts](licenses/version_check-0.9.5/) |
| vsimd-0.8.0 | MIT | [texts](licenses/vsimd-0.8.0/) |
| walkdir-2.5.0 | Unlicense/MIT | [texts](licenses/walkdir-2.5.0/) |
| want-0.3.2 | MIT | [texts](licenses/want-0.3.2/) |
| wasi-0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | [texts](licenses/wasi-0.11.1+wasi-snapshot-preview1/) |
| wasip2-1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | [texts](licenses/wasip2-1.0.4+wasi-0.2.12/) |
| wasm-bindgen-0.2.130 | MIT OR Apache-2.0 | [texts](licenses/wasm-bindgen-0.2.130/) |
| wasm-bindgen-futures-0.4.80 | MIT OR Apache-2.0 | [texts](licenses/wasm-bindgen-futures-0.4.80/) |
| wasm-bindgen-macro-0.2.130 | MIT OR Apache-2.0 | [texts](licenses/wasm-bindgen-macro-0.2.130/) |
| wasm-bindgen-macro-support-0.2.130 | MIT OR Apache-2.0 | [texts](licenses/wasm-bindgen-macro-support-0.2.130/) |
| wasm-bindgen-shared-0.2.130 | MIT OR Apache-2.0 | [texts](licenses/wasm-bindgen-shared-0.2.130/) |
| web-sys-0.3.107 | MIT OR Apache-2.0 | [texts](licenses/web-sys-0.3.107/) |
| web-time-1.1.0 | MIT OR Apache-2.0 | [texts](licenses/web-time-1.1.0/) |
| webpki-root-certs-1.0.12 | CDLA-Permissive-2.0 | [texts](licenses/webpki-root-certs-1.0.12/) |
| webpki-roots-1.0.12 | CDLA-Permissive-2.0 | [texts](licenses/webpki-roots-1.0.12/) |
| whoami-2.1.3 | Apache-2.0 OR BSL-1.0 OR MIT | [texts](licenses/whoami-2.1.3/) |
| winapi-util-0.1.11 | Unlicense OR MIT | [texts](licenses/winapi-util-0.1.11/) |
| windows-core-0.62.2 | MIT OR Apache-2.0 | [texts](licenses/windows-core-0.62.2/) |
| windows-implement-0.60.2 | MIT OR Apache-2.0 | [texts](licenses/windows-implement-0.60.2/) |
| windows-interface-0.59.3 | MIT OR Apache-2.0 | [texts](licenses/windows-interface-0.59.3/) |
| windows-link-0.2.1 | MIT OR Apache-2.0 | [texts](licenses/windows-link-0.2.1/) |
| windows-result-0.4.1 | MIT OR Apache-2.0 | [texts](licenses/windows-result-0.4.1/) |
| windows-strings-0.5.1 | MIT OR Apache-2.0 | [texts](licenses/windows-strings-0.5.1/) |
| windows-sys-0.52.0 | MIT OR Apache-2.0 | [texts](licenses/windows-sys-0.52.0/) |
| windows-sys-0.61.2 | MIT OR Apache-2.0 | [texts](licenses/windows-sys-0.61.2/) |
| windows-targets-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows-targets-0.52.6/) |
| windows_aarch64_gnullvm-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_aarch64_gnullvm-0.52.6/) |
| windows_aarch64_msvc-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_aarch64_msvc-0.52.6/) |
| windows_i686_gnu-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_i686_gnu-0.52.6/) |
| windows_i686_gnullvm-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_i686_gnullvm-0.52.6/) |
| windows_i686_msvc-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_i686_msvc-0.52.6/) |
| windows_x86_64_gnu-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_x86_64_gnu-0.52.6/) |
| windows_x86_64_gnullvm-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_x86_64_gnullvm-0.52.6/) |
| windows_x86_64_msvc-0.52.6 | MIT OR Apache-2.0 | [texts](licenses/windows_x86_64_msvc-0.52.6/) |
| wit-bindgen-0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | [texts](licenses/wit-bindgen-0.57.1/) |
| writeable-0.6.4 | Unicode-3.0 | [texts](licenses/writeable-0.6.4/) |
| yoke-0.8.4 | Unicode-3.0 | [texts](licenses/yoke-0.8.4/) |
| yoke-derive-0.8.4 | Unicode-3.0 | [texts](licenses/yoke-derive-0.8.4/) |
| zerocopy-0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | [texts](licenses/zerocopy-0.8.63/) |
| zerocopy-derive-0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | [texts](licenses/zerocopy-derive-0.8.63/) |
| zerofrom-0.1.8 | Unicode-3.0 | [texts](licenses/zerofrom-0.1.8/) |
| zerofrom-derive-0.1.8 | Unicode-3.0 | [texts](licenses/zerofrom-derive-0.1.8/) |
| zeroize-1.9.1 | Apache-2.0 OR MIT | [texts](licenses/zeroize-1.9.1/) |
| zerotrie-0.2.5 | Unicode-3.0 | [texts](licenses/zerotrie-0.2.5/) |
| zerovec-0.11.9 | Unicode-3.0 | [texts](licenses/zerovec-0.11.9/) |
| zerovec-derive-0.11.7 | Unicode-3.0 | [texts](licenses/zerovec-derive-0.11.7/) |
| zmij-1.0.23 | MIT | [texts](licenses/zmij-1.0.23/) |
