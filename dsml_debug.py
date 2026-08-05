"""Simulate the DSML interception flow to find the root cause"""
import os
os.chdir(r'D:\holoproxy')

with open('src/stream.rs', 'r', encoding='utf-8') as f:
    code = f.read()

# Extract valid_triggers from stream.rs source
import re
triggers = {}
for m in re.finditer(r'valid_triggers\.insert\("(.+?)"\.into\(\),\s*"(.+?)"\.into\(\)\);', code):
    open_tag, close_tag = m.group(1), m.group(2)
    triggers[open_tag] = close_tag
    print(f'Trigger: {repr(open_tag)} → {repr(close_tag)}')

# Now simulate a typical DSML model output chunk stream
# DeepSeek actual DSML output format:
dsml_stream = [
    "I'll help you run that command.\n\n",
    "<｜tool𠜁call𠜁begin｜>function<｜tool𠜁sep｜>Bash<｜tool𠜁call𠜁argument𠜁begin｜>{\"command\":\"ls\"}<｜tool𠜁call𠜁end｜>",
]

# Actually let's use the literal characters
# ｜ = U+FF5C
# ▁ = U+2581
sep = '▁'
pipe = '｜'

single_tool = f'<{pipe}tool{sep}call{sep}begin{pipe}>function<{pipe}tool{sep}sep{pipe}>Bash<{pipe}tool{sep}call{sep}argument{sep}begin{pipe}>{{"command":"ls"}}<{pipe}tool{sep}call{sep}end{pipe}>'

print(f"\n=== Model DSML output (simulated) ===")
print(f"repr: {repr(single_tool)}")
for ch in single_tool:
    if ord(ch) >= 128:
        print(f'  U+{ord(ch):04X} {ch}')
print()

# Check: does any trigger match?
print("=== Trigger matching test ===")
for open_tag, close_tag in triggers.items():
    if open_tag in single_tool:
        print(f'MATCH: {repr(open_tag)} at position {single_tool.find(open_tag)}')
    # Also check if trigger (without modification) matches
    # Check if there's a close tag in the text too
    if close_tag in single_tool and close_tag != open_tag:
        print(f'  close tag found: {repr(close_tag)} at {single_tool.find(close_tag)}')

print()
print("=== What triggers DON'T match (and why) ===")
for open_tag, close_tag in triggers.items():
    if open_tag not in single_tool:
        # Show what differs
        min_len = min(len(open_tag), 20)
        for i in range(min_len):
            if i < len(single_tool) and i < len(open_tag) and single_tool[i] != open_tag[i]:
                print(f'  {repr(open_tag[:i+1])} fails at char {i}: trigger={repr(open_tag[i])} model={repr(single_tool[i])}')
                break
        else:
            if len(open_tag) > min_len:
                print(f'  {repr(open_tag[:min_len])}... too long vs model text')
            else:
                print(f'  {repr(open_tag)} not found in text (positions exhausted)')
