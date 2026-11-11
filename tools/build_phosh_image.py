import os, glob, shutil, subprocess, sys
sys.path.insert(0,'/run/media/ivan/New Volume/My Projects/Silver microkernel/scripts')
from sfs_inject import Image

STAGE='/tmp/alarm/stage'
PKG='/tmp/alarm/phosh'
GTK4='/home/ivan/phosh-state/gtk4-root'
SRC='/home/ivan/phosh-state/disk_gtk3.img'
DST='/home/ivan/phosh-state/disk_phosh.img'

shutil.copyfile(SRC, DST)
img=Image(DST)

def root_names(d):
    out=set()
    for b in img.dir_blocks(d):
        s=b*img.block_size
        for slot in range(img.block_size//32):
            e=s+slot*32
            i=int.from_bytes(img.data[e:e+4],'little')
            if i: out.add(img.data[e+5:e+5+img.data[e+4]].decode('utf-8','replace'))
    return out
have=root_names(img.root)

libs={}
for base in (STAGE+'/usr/lib', STAGE+'/lib', STAGE+'/usr/lib64', STAGE+'/usr/lib/pulseaudio'):
    if os.path.isdir(base):
        for n in os.listdir(base):
            p=os.path.join(base,n)
            if os.path.isfile(p) or os.path.islink(p): libs.setdefault(n,p)

def needed(path):
    try: out=subprocess.check_output(['aarch64-linux-gnu-objdump','-p',path],text=True,stderr=subprocess.DEVNULL)
    except Exception: return []
    return [l.split()[-1] for l in out.splitlines() if 'NEEDED' in l]

closure=set()
queue=needed(PKG+'/usr/lib/phosh/phosh')
while queue:
    s=queue.pop()
    if s in closure: continue
    closure.add(s)
    if s in libs:
        for n in needed(libs[s]): queue.append(n)

def shorten(name):
    # lib<stem>.so[.ver] -> keep "lib" and the version, shorten the stem
    i=name.find('.so')
    stem=name[3:i]
    ver=name[i:]
    budget=27-3-len(ver)
    stem=stem[:max(4,budget)].rstrip('.-')
    return 'lib'+stem+ver

files={}
rename={}
for soname in sorted(closure):
    if soname in have: continue
    src=libs.get(soname)
    if src is None:
        print('MISSING', soname); continue
    data=open(os.path.realpath(src),'rb').read()
    dest=soname
    if len(dest.encode())>27:
        dest=shorten(soname)
        while dest in files or dest in have:
            dest=dest[:-1]+'x'
        rename[soname]=dest
        tmp='/tmp/alarm/t.elf'; open(tmp,'wb').write(data)
        subprocess.run(['patchelf','--set-soname',dest,tmp],check=True)
        data=open(tmp,'rb').read()
        print('RENAME', soname, '->', dest)
    files[dest]=data

# Arch binaries point at /lib/ld-linux-aarch64.so.1; the guest root is /sfs.
phosh_tmp='/tmp/alarm/phosh.patched'
import shutil as _sh
_sh.copyfile(PKG+'/usr/lib/phosh/phosh', phosh_tmp)
subprocess.run(['patchelf','--set-interpreter','/sfs/ld-linux-aarch64.so.1',
                '--set-rpath','/sfs', phosh_tmp], check=True)
files['phosh']=open(phosh_tmp,'rb').read()

count=0; skipped=0
for root,dirs,fs in os.walk(PKG+'/usr/share'):
    for f in fs:
        p=os.path.join(root,f)
        rel=os.path.relpath(p,PKG+'/usr/share')
        if rel.startswith('locale/'): continue
        parts=rel.split('/')
        if any(len(x.encode())>27 for x in parts):
            skipped+=1; continue
        files['share/'+rel]=open(p,'rb').read(); count+=1
print('phosh data files', count, 'skipped-long-name', skipped)

# glycin loaders live in usr/lib/glycin-loaders/2+ in the package but the
# library searches $XDG_DATA_DIRS/glycin-loaders/2+; put them where it looks.
for loader in ('glycin-image-rs', 'glycin-image-rs.real', 'glycin-svg',
               'glycin-heif', 'glycin-jxl'):
    src=os.path.join('/tmp/alarm/gly/usr/lib/glycin-loaders/2+', loader)
    if os.path.isfile(src):
        tmp='/tmp/alarm/loader.patched'
        _sh.copyfile(src, tmp)
        subprocess.run(['patchelf','--set-interpreter','/sfs/ld-linux-aarch64.so.1',
                        '--set-rpath','/sfs', tmp], check=True)
        files['share/glycin-loaders/2+/'+loader]=open(tmp,'rb').read()

# schemas: merge GTK4 root + staged packages + phosh, compile once
schema_dir='/tmp/alarm/schemas'; out_dir='/tmp/alarm/schema-out'
shutil.rmtree(schema_dir,ignore_errors=True); shutil.rmtree(out_dir,ignore_errors=True)
os.makedirs(schema_dir); os.makedirs(out_dir)
for base in (GTK4+'/usr/share/glib-2.0/schemas', STAGE+'/usr/share/glib-2.0/schemas', PKG+'/usr/share/glib-2.0/schemas'):
    for p in glob.glob(base+'/*'):
        if p.endswith(('.gschema.xml','.enums.xml','.override','.xml')):
            shutil.copy(p, schema_dir)
r=subprocess.run(['glib-compile-schemas','--targetdir',out_dir,schema_dir],capture_output=True,text=True)
print('compile-schemas rc', r.returncode, r.stderr.strip()[:300])
if r.returncode==0:
    files['gschemas.compiled']=open(out_dir+'/gschemas.compiled','rb').read()

# rewrite DT_NEEDED for renamed sonames in every ELF we ship
for name,data in list(files.items()):
    if data[:4]!=b'\x7fELF': continue
    tmp='/tmp/alarm/t2.elf'; open(tmp,'wb').write(data)
    for old,new in rename.items():
        subprocess.run(['patchelf','--replace-needed',old,new,tmp],check=False)
    files[name]=open(tmp,'rb').read()

# Wallpaper and symbolic icons. Icon names are longer than the SFS 27-byte
# directory entry, so they ship as one archive that phosh-serve unpacks into
# /tmp/xdg at startup (see tests/linux/wayland_real.c).
import subprocess as _sp
theme_dir = '/tmp/alarm/theme'
_sp.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                     'gen_theme_assets.py')],
        check=True, env=dict(os.environ, THEME_OUT=theme_dir))
files['share/backgrounds/werewolf.png'] = open(
    os.path.join(theme_dir, 'share/backgrounds/werewolf.png'), 'rb').read()
pack_path = '/tmp/alarm/theme.pack'
_sp.run([sys.executable, '-c',
         'import sys; sys.path.insert(0, %r); import gen_theme_assets as g; '
         'print(g.make_pack(%r))' % (os.path.dirname(os.path.abspath(__file__)), pack_path)],
        check=True, env=dict(os.environ, THEME_OUT=theme_dir))
files['theme.pack'] = open(pack_path, 'rb').read()

for name,data in files.items():
    img.write_file_at(name,data)
img.save()
print('WROTE', len(files), 'files,', sum(len(v) for v in files.values())//1048576, 'MiB')
