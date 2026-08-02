import os
os.chdir(r'D:\holoproxy')
full = '｜'   # U+FF5C fullwidth vertical bar

# ================== stream.rs ==================
with open('src/stream.rs', 'r', encoding='utf-8') as f:
    c = f.read()

# Fix 1: add fullwidth invoke trigger
old = '    valid_triggers.insert("<invoke".into(), "</invoke>".into());\n'
new = (f'    valid_triggers.insert("<invoke".into(), "</invoke>".into());\n'
       f'    valid_triggers.insert("<{full}invoke{full}".into(), "</{full}invoke{full}>".into());\n')
assert old in c, 'FIX1: old not found'
c = c.replace(old, new)

# Fix 2: search for both invoke variants
old = '    let lower_text = text.to_lowercase();\n    if let Some(invoke_start) = lower_text.find("<invoke") {'
new = (f'    let lower_text = text.to_lowercase();\n'
       f'    if let Some(invoke_start) = lower_text.find("<invoke").or_else(|| text.find("<{full}invoke{full}")) {{')
assert old in c, f'FIX2: old not found: {old!r}'
c = c.replace(old, new)

# Fix 3: search for both </invoke> close tags
old = ('        let invoke_end = if let Some(end_pos) = text[invoke_start..].find("</invoke>") {\n'
       '            invoke_start + end_pos\n'
       '        } else {\n'
       '            text.len()\n'
       '        };')
new = (f'        let invoke_end = if let Some(end_pos) = text[invoke_start..].find("</invoke>") {{\n'
       f'            invoke_start + end_pos\n'
       f'        }} else if let Some(end_pos) = text[invoke_start..].find("</{full}invoke{full}>") {{\n'
       f'            invoke_start + end_pos\n'
       f'        }} else {{\n'
       f'            text.len()\n'
       f'        }};')
assert old in c, 'FIX3: old not found'
c = c.replace(old, new)

# Fix 4: case-insensitive parameter search
old = 'while let Some(p_start) = invoke_body[search_from..].find("<parameter") {'
new = 'while let Some(p_start) = invoke_body[search_from..].to_lowercase().find("<parameter") {'
assert old in c, 'FIX4: old not found'
c = c.replace(old, new)

# Fix 5: case-insensitive close parameter search
old = 'let p_val = if let Some(close_p) = invoke_body[content_start..].find("</parameter>") {'
new = 'let p_val = if let Some(close_p) = invoke_body[content_start..].to_lowercase().find("</parameter>") {'
assert old in c, 'FIX5: old not found'
c = c.replace(old, new)

with open('src/stream.rs', 'w', encoding='utf-8', newline='\n') as f:
    f.write(c)
print('stream.rs done')

# ================== server.rs ==================
with open('src/server.rs', 'r', encoding='utf-8') as f:
    c2 = f.read()

# Fix 6: Remove parameter triggers, keep invoke only
old = (f'    triggers.insert("<invoke", "</invoke>");\n'
       f'    triggers.insert("<parameter", "</parameter>");\n'
       f'    triggers.insert("<{full}invoke{full}", "</{full}invoke{full}>");\n'
       f'    triggers.insert("<{full}parameter{full}", "</{full}parameter{full}>");')
new = (f'    triggers.insert("<invoke", "</invoke>");\n'
       f'    triggers.insert("<{full}invoke{full}", "</{full}invoke{full}>");')
assert old in c2, f'FIX6: old not found'
c2 = c2.replace(old, new)

with open('src/server.rs', 'w', encoding='utf-8', newline='\n') as f:
    f.write(c2)
print('server.rs done')
