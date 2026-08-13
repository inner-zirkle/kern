#!/usr/bin/env python3
"""Extract #[cfg(test)] blocks from Rust source files into tests/<file>_test.rs."""
import os, sys

os.chdir('/Users/feb/dev/kern')

def brace_depth_at(line, start_col=0):
    """Count { as +1, } as -1 starting from start_col."""
    d = 0
    for i in range(start_col, len(line)):
        if line[i] == '{': d += 1
        elif line[i] == '}': d -= 1
    return d

def extract_file(filepath, tests_dir):
    """Extract standalone test blocks from a .rs file. Returns number of blocks extracted."""
    with open(filepath) as f:
        lines = f.readlines()
    
    basename = os.path.basename(filepath)
    base = os.path.splitext(basename)[0]
    outpath = os.path.join(tests_dir, f'{base}_test.rs')
    
    # Track impl block depth
    impl_depth = 0
    
    # Find all #[cfg(test)] blocks with their brace ranges
    blocks = []  # [(start_line, end_line, inside_impl)]
    
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        
        # Track impl block depth
        if stripped.startswith('impl ') or stripped.startswith('impl<') or 'impl ' in stripped.split('{')[0]:
            impl_depth += line.count('{') - line.count('}')
        elif impl_depth > 0:
            impl_depth += line.count('{') - line.count('}')
            if impl_depth <= 0:
                impl_depth = 0
        
        # Find #[cfg(test)]
        if stripped.startswith('#[cfg(test)]') or stripped == '#[cfg(test)]':
            inside_impl = impl_depth > 0
            
            # Find the next non-blank, non-comment line
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
            
            # Check if this starts a brace block (mod, fn, etc. with {)
            if '{' in lines[j]:
                # Track braces from this line forward
                depth = brace_depth_at(lines[j])
                if depth > 0:
                    k = j + 1
                    while k < len(lines) and depth > 0:
                        depth += brace_depth_at(lines[k])
                        k += 1
                    if depth == 0:
                        blocks.append((i, k - 1, inside_impl))
                        i = k
                        continue
                    else:
                        # Unclosed block — skip
                        blocks.append((i, j, inside_impl))
                        i += 1
                        continue
                else:
                    # Single-line {} — wait, this shouldn't happen with depth>0
                    pass
            elif lines[j].strip().endswith(';'):
                # Attribute on a single statement
                blocks.append((i, j, inside_impl))
            else:
                # Non-brace item — treat as single-line
                blocks.append((i, j, inside_impl))
        
        i += 1
    
    standalone = [(s, e) for s, e, impl in blocks if not impl]
    
    if not standalone:
        return 0  # nothing to extract
    
    # Build test content
    content = f'//! Tests extracted from {basename}\n'
    content += '#![allow(unused)]\n'
    content += 'use super::*;\n\n'
    
    for s, e in standalone:
        for li in range(s + 1, e + 1):  # skip #[cfg(test)], include rest
            content += lines[li]
    
    os.makedirs(tests_dir, exist_ok=True)
    with open(outpath, 'w') as f:
        f.write(content)
    
    # Remove standalone blocks from source (reverse order)
    new_lines = list(lines)
    for s, e in reversed(standalone):
        del new_lines[s:e + 1]
    
    # Insert replacement at the first removed block's position
    replacement = f'#[cfg(test)]\n#[path = "tests/{base}_test.rs"]\nmod {base}_tests;\n'
    insert_pos = standalone[0][0]
    new_lines.insert(insert_pos, replacement + '\n')
    
    with open(filepath, 'w') as f:
        f.writelines(new_lines)
    
    return len(standalone)

# Process all crates
total_blocks = 0
for crate in sorted(os.listdir('src')):
    src_dir = f'src/{crate}/src'
    if not os.path.isdir(src_dir):
        continue
    
    tests_dir = os.path.join(src_dir, 'tests')
    if os.path.exists(tests_dir) and os.listdir(tests_dir):
        print(f'  SKIP {crate} (already has tests/)')
        continue
    
    crate_blocks = 0
    for filename in sorted(os.listdir(src_dir)):
        if not filename.endswith('.rs') or filename == 'lib.rs':
            continue
        filepath = os.path.join(src_dir, filename)
        with open(filepath) as f:
            if '#[cfg(test)]' not in f.read():
                continue
        
        n = extract_file(filepath, tests_dir)
        if n > 0:
            crate_blocks += n
            print(f'  {filepath}: {n} block(s)')
        else:
            print(f'  {filepath}: all inside impl, skipped')
    
    if crate_blocks:
        total_blocks += crate_blocks

print(f'\nTotal: {total_blocks} blocks extracted')

# Verify compilation
import subprocess
print('\n--- cargo check ---')
result = subprocess.run(['cargo', 'check'], capture_output=True, text=True)
if result.returncode == 0:
    print('OK')
else:
    # Print only errors
    for line in result.stderr.split('\n'):
        if 'error' in line.lower():
            print(line)
    # Also check stdout
    for line in result.stdout.split('\n'):
        if 'error' in line.lower():
            print(line)
    print(f'\nExit: {result.returncode}')