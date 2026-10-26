import struct
for name in ["sl","block_driver"]:
    p="target/userspace/aarch64-unknown-none/debug/"+name
    d=open(p,"rb").read()
    e_entry,e_phoff=struct.unpack_from("<QQ",d,24)
    e_phentsize,e_phnum=struct.unpack_from("<HH",d,54)
    print("====",name,"size",len(d),"entry",hex(e_entry),"phnum",e_phnum)
    for i in range(e_phnum):
        off=e_phoff+i*e_phentsize
        t,fl,po,va,pa,fsz,msz,al=struct.unpack_from("<IIQQQQQQ",d,off)
        print("  type",t,"flags",fl,"off",hex(po),"va",hex(va),"fsz",fsz,"msz",msz,"align",hex(al))
