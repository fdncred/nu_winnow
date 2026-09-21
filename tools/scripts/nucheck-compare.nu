#!/usr/bin/env nu
# Compare the accept/reject verdicts of `nu-check` and nu-winnow-parser over
# every `.nu` file below the given directories.
#
# Requires the example binary: `cargo build --release --example parse`.
#
#   nu tools/scripts/nucheck-compare.nu ~/src/nu_scripts ~/src/nushell/crates/nu-std
#   nu tools/scripts/nucheck-compare.nu --details ~/src/nu_scripts | where {|r| $r.accepted_by_nu and not $r.accepted_by_ours }

# Record, for each file, whether `nu-check` and nu-winnow-parser accept it.
def main [
    --parse: path      # the `parse` example binary (default: target/release/examples/parse)
    --details          # return the full table instead of printing a summary
    ...dirs: path      # directories to search
]: nothing -> any {
    let parse = $parse | default ($env.FILE_PWD | path join ../../target/release/examples/parse | path expand)
    let files = $dirs | each {|d| glob ($d | path join '**/*.nu') } | flatten | sort
    let rows = $files | each {|f|
        {
            accepted_by_nu: (nu-check $f)
            accepted_by_ours: ((^$parse --check --quiet $f | complete | get exit_code) == 0)
            file: $f
        }
    }
    if $details {
        return $rows
    }
    let bugs = $rows | where {|r| $r.accepted_by_nu and not $r.accepted_by_ours }
    let lenient = $rows | where {|r| $r.accepted_by_ours and not $r.accepted_by_nu }
    let agree = $rows | where {|r| $r.accepted_by_nu == $r.accepted_by_ours }
    print $"($rows | length) files: ($agree | length) agree, ($bugs | length) accepted by nu but rejected here, ($lenient | length) rejected by nu but accepted here"
    if ($bugs | is-not-empty) {
        print "accepted by nu but rejected here (parser bugs):"
        $bugs | get file | each { print $"  ($in)" } | ignore
    }
    if ($lenient | is-not-empty) {
        print "rejected by nu but accepted here (usually semantic errors: missing modules, types):"
        $lenient | get file | each { print $"  ($in)" } | ignore
    }
}
