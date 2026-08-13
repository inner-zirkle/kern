#!/usr/bin/env python3
"""Extract inline #[cfg(test)] blocks from Rust source files into tests/<file>_test.rs.

Handles:
- impl-nested #[cfg(test)] — LEAVE in place
- Already-extracted files (have #[path = ...]) — SKIP
- Test-only files (name contains "test") — SKIP
- Multiple standalone #[cfg(test)] blocks — merge into one test file
"""

import os, sys

os.chdir('/Users/feb/dev/kern')
SKIP_CRATES = set()  # populated as we go

def brace_balance(line):
    """Return net brace balance of line. Comments and strings are not stripped."""
    return line.count('{') - line.count('}')

def extract_file(filepath, tests_dir):
    """Returns (test_file_path, num_blocks) or (None, 0) if nothing extracted."""
    with open(filepath) as f:
        lines = f.readlines()

    basename = os.path.basename(filepath)
    base = os.path.splitext(basename)[0]
    outpath = os.path.join(tests_dir, f'{base}_test.rs')

    if os.path.exists(outpath):
        return None, 0  # already done

    # Skip test-only files
    if 'test' in base.lower() or basename.startswith('test_'):
        return None, 0

    # Check if already using #[path] pattern
    for line in lines:
        if '#[path' in line and ('tests/' in line or 'test_' in line):
            return None, 0  # already extracted

    # ---- Phase 1: Find all #[cfg(test)] blocks ----
    blocks = []  # [(start_line, end_line, inside_impl, content_mod_name)]
    impl_depth = 0

    i = 0
    while i < len(lines):
        stripped = lines[i].strip()

        # Track impl depth FIRST (before checking this line for cfg(test))
        # But only if this line isn't itself an impl start
        if impl_depth > 0:
            impl_depth += brace_balance(lines[i])
            if impl_depth < 0:
                impl_depth = 0
        elif stripped.startswith('impl ') or stripped.startswith('impl<') or stripped.split('{')[0].strip().endswith(' impl'):
            # Line starts an impl block
            impl_depth += brace_balance(lines[i])

        # Find #[cfg(test)]
        if stripped.startswith('#[cfg(test)]') or stripped == '#[cfg(test)]':
            inside = impl_depth > 0

            # Find next non-blank/non-comment line
            j = i + 1
            while j < len(lines):
                ns = lines[j].strip()
                if ns == '' or ns.startswith('//') or ns.startswith('/*'):
                    j += 1
                else:
                    break
            if j >= len(lines):
                i += 1
                continue

            # Does next line open a brace block?
            if '{' in lines[j]:
                # Count braces from this line forward
                depth = brace_balance(lines[j])
                k = j + 1
                while k < len(lines) and depth > 0:
                    depth += brace_balance(lines[k])
                    k += 1
                if depth == 0:
                    blocks.append((i, k - 1, inside))
                    i = k
                    continue
                else:
                    # Unclosed — treat as single-line
                    blocks.append((i, j, inside))
            elif lines[j].strip().endswith(';'):
                # Statement with #[cfg(test)] attribute
                blocks.append((i, j, inside))
            else:
                # Non-brace item
                blocks.append((i, j, inside))

        i += 1

    # Separate impl vs standalone
    standalone = [(s, e) for s, e, impl in blocks if not impl]
    impl_nested = [(s, e) for s, e, impl in blocks if impl]

    if not standalone:
        return None, 0

    print(f"    {len(standalone)} standalone + {len(impl_nested)} impl-nested")

    # ---- Phase 2: Build test file ----
    content = f'//! Tests extracted from {basename}\n'
    content += '#![allow(unused)]\n'
    content += 'use super::*;\n\n'

    for s, e in standalone:
        for li in range(s + 1, e + 1):
            content += lines[li]

    os.makedirs(tests_dir, exist_ok=True)
    with open(outpath, 'w') as f:
        f.write(content)

    # ---- Phase 3: Update source ----
    new_lines = list(lines)
    for s, e in reversed(standalone):
        del new_lines[s:e + 1]

    # Insert replacement
    repl = f'#[cfg(test)]\n#[path = "tests/{base}_test.rs"]\nmod {base}_tests;\n'
    insert_pos = standalone[0][0]
    new_lines.insert(insert_pos, repl + '\n')

    # Remove trailing blank lines from insertion that might cause fmt issues
    # (fmt handles this fine)

    with open(filepath, 'w') as f:
        f.writelines(new_lines)

    return outpath, len(standalone)


# ---- Main ----
total = 0
for crate in sorted(os.listdir('src')):
    src_dir = f'src/{crate}/src'
    if not os.path.isdir(src_dir):
        continue
    if crate in SKIP_CRATES:
        continue

    tests_dir = os.path.join(src_dir, 'tests')
    crate_total = 0

    for filename in sorted(os.listdir(src_dir)):
        if not filename.endswith('.rs') or filename == 'lib.rs':
            continue
        filepath = os.path.join(src_dir, filename)
        if '#[cfg(test)]' not in open(filepath).read():
            continue

        result = extract_file(filepath, tests_dir)
        if result[0]:
            print(f'  {filepath}: {result[1]} block(s)')
            crate_total += result[1]

    if crate_total:
        total += crate_total
        print(f'  -> {crate}: {crate_total} blocks')

print(f'\nTotal: {total} blocks extracted')

# Verify
import subprocess
print('\n--- cargo check ---')
r = subprocess.run(['cargo', 'check'], capture_output=True, text=True)
if r.returncode == 0:
    print('OK')
else:
    for line in (r.stderr + r.stdout).split('\n'):
        if 'error' in line.lower() and 'warning' not in line.lower():
            print(line)