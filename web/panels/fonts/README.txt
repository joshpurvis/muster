Fonts bundled into every Muster panel (the built-in apps and panels opened with kit: true).
They are inlined as data: URIs when a panel's HTML is served (src/panel_kit.rs; the mock daemon
uses web/dev/build_panels.ts), so a panel never loads anything from the network.

- inter-latin.woff2: Inter, Latin subset, variable weight. SIL Open Font License 1.1, see OFL-Inter.txt.
- jetbrains-mono-latin.woff2: JetBrains Mono, Latin subset, variable weight. SIL Open Font License 1.1,
  see OFL-JetBrainsMono.txt.
