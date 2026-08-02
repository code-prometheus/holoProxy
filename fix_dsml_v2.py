# -*- coding: utf-8 -*-
"""
Fix DSML handling in holoproxy:
1. Add fullwidth pipe invoke trigger in both paths
2. parse_fallback_tool: case-insensitive + fullwidth variant search
3. Remove parameter triggers (only invoke triggers interception)
"""
import os, sys
os.chdir(r'D:\holoproxy')
FS = '｜'  # U+FF5C

def fix_stream_rs():
    with open('src/stream.rs', 'r', encoding='utf-8') as f:
        c = f.read()

    checks = []

    # 1. Add fullwidth invoke trigger (fix comment too)
    old = '    // invoke/parameter XML 格式\n    valid_triggers.insert("<invoke".into(), "</invoke>".into());\n'
    new = f'    // invoke XML format (regular + fullwidth pipe)\n    valid_triggers.insert("<invoke".into(), "</invoke>".into());\n    valid_triggers.insert("<{FS}invoke{FS}".into(), "</{FS}invoke{FS}>".into());\n'
    assert old in c, f'FAIL: trigger insert not found'
    c = c.replace(old, new)
    checks.append('1. trigger insert')

    # 2. parse_fallback_tool: search both invoke variants
    old = 'let lower_text = text.to_lowercase();\n    if let Some(invoke_start) = lower_text.find("<invoke") {'
    new = f'let lower_text = text.to_lowercase();\n    if let Some(invoke_start) = lower_text.find("<invoke").or_else(|| text.find("<{FS}invoke{FS}")) {{'
    assert old in c, f'FAIL: invoke search not found'
    c = c.replace(old, new)
    checks.append('2. dual invoke search')

    # 3. Close tag: both </invoke> variants
    old = '        let invoke_end = if let Some(end_pos) = text[invoke_start..].find("</invoke>") {\n            invoke_start + end_pos\n        } else {\n            text.len()\n        };'
    new = f'        let invoke_end = if let Some(end_pos) = text[invoke_start..].find("</invoke>") {{\n            invoke_start + end_pos\n        }} else if let Some(end_pos) = text[invoke_start..].find("</{FS}invoke{FS}>") {{\n            invoke_start + end_pos\n        }} else {{\n            text.len()\n        }};'
    assert old in c, f'FAIL: invoke close not found'
    c = c.replace(old, new)
    checks.append('3. dual invoke close')

    # 4. Case-insensitive parameter search
    old = 'while let Some(p_start) = invoke_body[search_from..].find("<parameter") {'
    new = 'while let Some(p_start) = invoke_body[search_from..].to_lowercase().find("<parameter") {'
    assert old in c, f'FAIL: parameter search not found'
    c = c.replace(old, new)
    checks.append('4. ci parameter search')

    # 5. Case-insensitive close parameter
    old = 'let p_val = if let Some(close_p) = invoke_body[content_start..].find("</parameter>") {'
    new = 'let p_val = if let Some(close_p) = invoke_body[content_start..].to_lowercase().find("</parameter>") {'
    assert old in c, f'FAIL: close parameter search not found'
    c = c.replace(old, new)
    checks.append('5. ci close parameter')

    with open('src/stream.rs', 'w', encoding='utf-8') as f:
        f.write(c)
    for chk in checks:
        print(f'  [OK] {chk}')
    print('stream.rs complete')

def fix_server_rs():
    with open('src/server.rs', 'r', encoding='utf-8') as f:
        c = f.read()

    # Remove parameter triggers, keep invoke only
    old = f'    triggers.insert("<invoke", "</invoke>");\n    triggers.insert("<parameter", "</parameter>");\n    triggers.insert("<{FS}invoke{FS}", "</{FS}invoke{FS}>");\n    triggers.insert("<{FS}parameter{FS}", "</{FS}parameter{FS}>");'
    new = f'    triggers.insert("<invoke", "</invoke>");\n    triggers.insert("<{FS}invoke{FS}", "</{FS}invoke{FS}>");'
    assert old in c, f'FAIL: server triggers not found'
    c = c.replace(old, new)

    with open('src/server.rs', 'w', encoding='utf-8') as f:
        f.write(c)
    print('server.rs complete')

if __name__ == '__main__':
    fix_stream_rs()
    fix_server_rs()
    print('All DSML fixes applied successfully!')
