# Contributing

Thanks for helping improve gphotos-app. It is a GPL-3.0 project; by contributing
you agree that your work is licensed the same way.

## Before you start

- Read `README.md` for the build/run steps and `companion/README.md` for the
  helper scripts the app needs.
- Read `LIMITATIONS.md` for limitations that are deliberate (for
  example, features that need private Google RPCs are not faked).
- Open an issue first for anything large, so we can agree on the approach.

## Making a change

```bash
source ~/.cargo/env                 # if cargo is not on your PATH
cargo build --release
cargo test --bin gphotos-app        # offline unit tests
./target/release/gphotos-app --selftest   # needs a signed-in browser session
```

Keep changes small and focused. The UI uses a hand-drawn paper/ink style;
please preserve it. Add no new dependency without discussing it in an issue.

## Reporting bugs

Include your OS, Rust and browser versions, the exact steps to reproduce, and
what you expected. Paste log text only after removing cookie values, signed
media URLs, album names and email addresses — these are credentials or private
data, even in a bug report. Never attach a session file or a `urls.tsv`.

## Commits and pull requests

- One logical change per commit, with a short imperative message.
- Run the test commands above and say exactly what you ran and what passed.
- If you changed behaviour, update `README.md` and `LIMITATIONS.md`.
- Do not commit secrets, generated files, screenshots or local paths.
