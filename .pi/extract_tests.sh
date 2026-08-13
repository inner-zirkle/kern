#!/usr/bin/env bash
# Extract #[cfg(test)] mod tests { ... } blocks from source files into tests/<module>_test.rs
set -euo pipefail

CRATE="$1"  # e.g., "src/graph"
SRC_DIR="$CRATE/src"
TESTS_DIR="$SRC_DIR/tests"

mkdir -p "$TESTS_DIR"

cd /Users/feb/dev/kern

# Find source files with #[cfg(test)]
for src_file in $(grep -rl '#\[cfg(test)\]' "$SRC_DIR" --include="*.rs" | sort); do
    basename=$(basename "$src_file" .rs)
    
    # Skip lib.rs — tests stay there
    if [ "$basename" = "lib" ]; then
        echo "  skip lib.rs"
        continue
    fi
    
    # Count #[cfg(test)] occurrences in this file
    count=$(grep -c '#\[cfg(test)\]' "$src_file")
    echo "  $src_file ($count #[cfg(test)] blocks)"
    
    # Count mod tests blocks
    mod_count=$(grep -c '#\[cfg(test)\].*mod.*tests\b' "$src_file" || true)
    if [ "$mod_count" -gt 1 ]; then
        echo "    WARNING: multiple test modules, need manual handling"
        continue
    fi
    
    # Extract the #[cfg(test)] mod tests { ... } block
    # Use awk to find the block: from #[cfg(test)] line to matching }
    awk -v out="$TESTS_DIR/${basename}_test.rs" '
    BEGIN { in_test=0; brace_depth=0; found_cfg=0; output=""; line_start=0 }
    
    /#\[cfg\(test\)\]/ && !found_cfg {
        # Check if next line is "mod tests {" or similar
        found_cfg=1
        cfg_line=NR
        next
    }
    
    found_cfg && !in_test {
        if ($0 ~ /^[[:space:]]*mod[[:space:]]+[a-zA-Z_]+[[:space:]]*\{/) {
            in_test=1
            brace_depth=1
            # Store the output header
            output = "//! Tests extracted from " FILENAME "\n"
            output = output "#![allow(unused)]\n"
            output = output "use super::*;\n"
            line_start=NR
            next
        } else if (/^[[:space:]]*pub[[:space:]]+fn/ || /^[[:space:]]*fn/ || /^[[:space:]]*use/ || /^[[:space:]]*const/ || /^[[:space:]]*static/) {
            # Standalone #[cfg(test)] item — skip for now
            found_cfg=0
            next
        }
    }
    
    in_test {
        # Count braces to find matching }
        n = split($0, chars, "")
        for (i in chars) {
            if (chars[i] == "{") brace_depth++
            if (chars[i] == "}") brace_depth--
        }
        output = output $0 "\n"
        if (brace_depth == 0) {
            # Write the output file
            print output > out
            print "    -> " out
            in_test=0
            found_cfg=0
        }
        next
    }
    ' "$src_file"
done