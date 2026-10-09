#!/usr/bin/env python3
"""Static cross-checks (no compiler needed): Slint <-> Rust <-> Kotlin consistency."""
import re, glob, sys
bad=0
def err(m):
    global bad; bad+=1; print('  ERROR',m)
def strip_slint(s): return re.sub(r'//[^\n]*','',re.sub(r'"(\\.|[^"\\])*"','""',s))
state=open('ui/state.slint').read()
props=set(m.replace('-','_') for m in re.findall(r'in-out property <[^>]+>\s+([a-z0-9-]+)',state))
cbs=set(m.replace('-','_') for m in re.findall(r'(?:pure\s+)?callback\s+([a-z0-9-]+)',state))
print('AppState: %d properties, %d callbacks'%(len(props),len(cbs)))
# 1) Rust -> AppState
rust=''.join(open(f).read() for f in glob.glob('src/*.rs'))
used=set(re.findall(r'\.(?:set|get|on)_([a-z0-9_]+)\(',rust))
def known(n): return n in props or n in cbs or ('on_'+n) in [] 
rs_calls=re.findall(r'\.(set|get|on)_([a-z0-9_]+)\(',rust)
miss=set()
for kind,n in rs_calls:
    if kind=='on' and n in cbs: continue
    if kind in('set','get') and n in props: continue
    miss.add((kind,n))
# calls that belong to other APIs (not AppState) are whitelisted
other={'set','get','on'}
whitelist={('set','all'),('get','all')}
for kind,n in sorted(miss):
    if (kind,n) in whitelist: continue
    print('  note: %s_%s is not an AppState member (ok only if it belongs to another API)'%(kind,n))
# every callback should be handled in Rust
for c in sorted(cbs):
    if 'on_'+c not in rust and c!='cover_image': err('callback %s is never handled in Rust (on_%s)'%(c,c))
# every property that Rust sets exists; list properties never touched by Rust (info)
untouched=[p for p in sorted(props) if ('set_'+p) not in rust and ('get_'+p) not in rust]
print('  properties only driven by the UI itself:',', '.join(untouched))
# 2) Slint -> Slint
ui=''.join(strip_slint(open(f).read()) for f in glob.glob('ui/*.slint') if not f.endswith('state.slint'))
for m in sorted(set(re.findall(r'AppState\.([a-z0-9-]+)',ui))):
    if m.replace('-','_') not in props and m.replace('-','_') not in cbs: err('AppState.%s used in Slint but not declared'%m)
ic=set(re.findall(r'out property <string> ([a-z0-9]+):',open('ui/icons_data.slint').read()))
for m in sorted(set(re.findall(r'\bIc\.([a-z0-9]+)',ui))):
    if m not in ic: err('Ic.%s missing in icons_data.slint'%m)
theme=open('ui/theme.slint').read()
tp=set(re.findall(r'out property <[^>]+> ([a-z0-9-]+):',theme))
for m in sorted(set(re.findall(r'\bTheme\.([a-z0-9-]+)',ui))):
    if m not in tp: err('Theme.%s missing in theme.slint'%m)
tokens=set(re.findall(r'out property <\[color\]> ([a-z0-9]+):',open('ui/tokens.slint').read()))
for m in sorted(set(re.findall(r'Pal\.([a-z0-9]+)\[',theme))):
    if m not in tokens: err('Pal.%s missing in tokens.slint'%m)
app=strip_slint(open('ui/app.slint').read())
names=set(re.findall(r'out property <\[string\]> ([a-z]+):',app))
for m in sorted(set(re.findall(r'\bNames\.([a-z]+)',app))):
    if m not in names: err('Names.%s missing'%m)
# component references
defined=set(re.findall(r'(?:export\s+)?(?:component|global)\s+([A-Za-z0-9]+)',''.join(strip_slint(open(f).read()) for f in glob.glob('ui/*.slint'))))
builtin={'Rectangle','Image','Text','TextInput','TouchArea','Flickable','Path','Window','VerticalLayout','HorizontalLayout','GridLayout','ListRow','PlCard','PickRow'}
for f in glob.glob('ui/*.slint'):
    s=strip_slint(open(f).read())
    for m in set(re.findall(r'(?:^|[\s{:])([A-Z][A-Za-z0-9]+)\s*\{',s,flags=re.M)):
        if m not in defined and m not in builtin: err('%s: element %s is not defined/imported'%(f,m))
# imports provide every used component
for f in ('ui/app.slint',):
    s=strip_slint(open(f).read()); imp=set()
    for m in re.findall(r'import\s*\{([^}]*)\}',s): imp|={x.strip() for x in m.split(',')}
    for m in set(re.findall(r'(?:^|[\s{:])([A-Z][A-Za-z0-9]+)\s*\{',s,flags=re.M)):
        local=set(re.findall(r'(?:component|global)\s+([A-Za-z0-9]+)',s))
        if m not in imp and m not in local and m not in builtin: err('app.slint uses %s but does not import it'%m)
# 3) balance
def bal(name,s,pairs='(){}[]'):
    s=re.sub(r'"(\\.|[^"\\])*"','""',s); s=re.sub(r'//[^\n]*','',s)
    for o,c in ('()','{}','[]'):
        if s.count(o)!=s.count(c): err('%s: unbalanced %s%s (%d/%d)'%(name,o,c,s.count(o),s.count(c)))
for f in glob.glob('ui/*.slint')+glob.glob('src/*.rs')+glob.glob('android/app/src/main/kotlin/dev/velora/player/*.kt'):
    s=open(f).read()
    if f.endswith('.rs'): s=re.sub(r"'(\\.|[^'\\])'","''",s)
    bal(f,s)
# 4) JNI contract
kt=open('android/app/src/main/kotlin/dev/velora/player/MediaBridge.kt').read()
ktm=set(re.findall(r'@JvmStatic\s+(?:external\s+)?fun\s+([A-Za-z]+)',kt))
rsm=set(re.findall(r'call_static_method\(\s*cls,\s*"([A-Za-z]+)"',open('src/android.rs').read()))
for m in rsm:
    if m not in ktm: err('Rust calls MediaBridge.%s but Kotlin does not declare it'%m)
exp=re.findall(r'pub extern "system" fn (Java_[A-Za-z0-9_]+)',open('src/android.rs').read())
for e in exp:
    if e!='Java_dev_velora_player_MediaBridge_nativeCommand': err('unexpected JNI export '+e)
    if 'external fun nativeCommand' not in kt: err('Kotlin has no external nativeCommand')
print('JNI: Rust->Kotlin methods',sorted(rsm),'| exported to Kotlin',exp)
# command code table agrees
rs_codes=dict((int(a),b) for a,b in re.findall(r'(\d)\s*=>\s*NativeCmd::([A-Za-z]+)',open('src/android.rs').read()))
kt_codes=dict((b,int(a)) for b,a in re.findall(r'const val CMD_([A-Z_]+) = (\d)',kt))
m={'PLAY':'Play','PAUSE':'Pause','TOGGLE':'Toggle','NEXT':'Next','PREV':'Prev','SEEK':'SeekMs','STOP':'Stop','RESUME':'Resume','PERMISSION_GRANTED':'PermissionGranted'}
for k,v in kt_codes.items():
    if rs_codes.get(v)!=m[k]: err('command code mismatch: Kotlin CMD_%s=%d vs Rust %s'%(k,v,rs_codes.get(v)))
print('command codes:',kt_codes)
print('RESULT:', 'OK' if bad==0 else '%d problem(s)'%bad)
sys.exit(1 if bad else 0)
