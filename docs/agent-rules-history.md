# Why the agent rules say what they say

Background for rules in `AGENTS.md`, kept here so that file stays short. Each entry names the rule and what happened.

## Screenshots of the UI: desktop pictures come from WebKitGTK

Headless Chromium hides bugs the desktop window shows: a dropdown whose text was cut off in the window looked right in
Chromium and was "fixed" twice before anyone saw it there. Since then every desktop picture for a pull request,
`docs/screenshots/` or a design review comes from WebKitGTK through `deno task shot:desktop`.
