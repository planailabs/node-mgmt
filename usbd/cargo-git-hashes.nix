# Output hashes for the git deps in usbd/Cargo.lock (the dioxus forks
# memvault-web pins). Parameterized so flake.nix supplies the same
# dioxusHash/dioxusI18nHash it already uses for the SPA build.
{ dioxusHash, dioxusI18nHash }:
{
  "const-serialize-0.8.0-alpha.0" = dioxusHash;
  "const-serialize-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-0.8.0-alpha.0" = dioxusHash;
  "dioxus-asset-resolver-0.8.0-alpha.0" = dioxusHash;
  "dioxus-cli-config-0.8.0-alpha.0" = dioxusHash;
  "dioxus-config-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-config-macros-0.8.0-alpha.0" = dioxusHash;
  "dioxus-core-0.8.0-alpha.0" = dioxusHash;
  "dioxus-core-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-core-types-0.8.0-alpha.0" = dioxusHash;
  "dioxus-devtools-0.8.0-alpha.0" = dioxusHash;
  "dioxus-devtools-types-0.8.0-alpha.0" = dioxusHash;
  "dioxus-document-0.8.0-alpha.0" = dioxusHash;
  "dioxus-fullstack-0.8.0-alpha.0" = dioxusHash;
  "dioxus-fullstack-core-0.8.0-alpha.0" = dioxusHash;
  "dioxus-fullstack-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-history-0.8.0-alpha.0" = dioxusHash;
  "dioxus-hooks-0.8.0-alpha.0" = dioxusHash;
  "dioxus-html-0.8.0-alpha.0" = dioxusHash;
  "dioxus-html-internal-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-interpreter-js-0.8.0-alpha.0" = dioxusHash;
  "dioxus-liveview-0.8.0-alpha.0" = dioxusHash;
  "dioxus-logger-0.8.0-alpha.0" = dioxusHash;
  "dioxus-router-0.8.0-alpha.0" = dioxusHash;
  "dioxus-router-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-rsx-0.8.0-alpha.0" = dioxusHash;
  "dioxus-server-0.8.0-alpha.0" = dioxusHash;
  "dioxus-signals-0.8.0-alpha.0" = dioxusHash;
  "dioxus-ssr-0.8.0-alpha.0" = dioxusHash;
  "dioxus-stores-0.8.0-alpha.0" = dioxusHash;
  "dioxus-stores-macro-0.8.0-alpha.0" = dioxusHash;
  "dioxus-web-0.8.0-alpha.0" = dioxusHash;
  "generational-box-0.8.0-alpha.0" = dioxusHash;
  "lazy-js-bundle-0.8.0-alpha.0" = dioxusHash;
  "manganis-0.8.0-alpha.0" = dioxusHash;
  "manganis-core-0.8.0-alpha.0" = dioxusHash;
  "manganis-macro-0.8.0-alpha.0" = dioxusHash;
  "subsecond-0.8.0-alpha.0" = dioxusHash;
  "subsecond-types-0.8.0-alpha.0" = dioxusHash;
  "dioxus-i18n-0.5.1" = dioxusI18nHash;
}
