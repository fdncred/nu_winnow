# Comparison scripts (Nushell 0.115)

All scripts run with `nu` 0.115.2 and need the example binary built with
`cargo build --release --example parse`.

* `nucheck-compare.nu DIR...` — for every `.nu` file, records whether
  `nu-check` and this parser accept it, and prints the disagreements. With
  `--details` it returns the full table for further querying.
* `flatcmp.nu FILE...` — compares nu's `ast --flatten` classification with
  this crate's `flatten()` segment by segment, after mapping both to a coarse
  class alphabet (call, string, var, literal, op, flag, delim, sig). Pass
  `--commands std_commands.txt` so standard-library commands such as
  `assert equal` resolve as multi-word names, as they do inside nu.
* `gen-std-commands.nu [STD_DIR]` — regenerates `std_commands.txt`, the list
  of standard-library exports both bare and module-prefixed.

```text
nu tools/scripts/nucheck-compare.nu ~/src/nu_scripts ~/src/nushell/crates/nu-std
nu tools/scripts/flatcmp.nu --commands tools/scripts/std_commands.txt ...(glob ~/src/nushell/crates/nu-std/**/*.nu)
nu tools/scripts/gen-std-commands.nu ~/src/nushell/crates/nu-std | save -f tools/scripts/std_commands.txt
```
