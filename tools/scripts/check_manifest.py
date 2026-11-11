import struct, sys

def rva_to_off(d, rva, sections):
    for name, vsize, voff, rsize, roff, _ in sections:
        if voff <= rva < voff + max(vsize, rsize):
            return roff + (rva - voff)
    return None

def parse(d):
    e_lfanew = struct.unpack_from('<I', d, 0x3c)[0]
    assert d[e_lfanew:e_lfanew+4] == b'PE\x00\x00'
    coff = e_lfanew + 4
    nsec = struct.unpack_from('<H', d, coff+2)[0]
    optsize = struct.unpack_from('<H', d, coff+16)[0]
    opt = coff + 20
    magic = struct.unpack_from('<H', d, opt)[0]
    dd = opt + (112 if magic == 0x20b else 96)
    res_rva, res_size = struct.unpack_from('<II', d, dd + 2*8)  # index 2 = resource
    # sections
    sh = opt + optsize
    sections = []
    for i in range(nsec):
        o = sh + i*40
        name = d[o:o+8]
        vsize, voff, rsize, roff = struct.unpack_from('<IIII', d, o+8)
        sections.append((name, vsize, voff, rsize, roff, o))
    res_off = rva_to_off(d, res_rva, sections)
    def walk(off, level):
        nname, nid = struct.unpack_from('<HH', d, off)
        cnt = nname + nid
        ents = []
        for i in range(cnt):
            eo = off + 16 + i*8
            name_id, offdata = struct.unpack_from('<II', d, eo)
            ents.append((name_id, offdata))
        if level == 0:
            # find RT_MANIFEST (24)
            for nid, od in ents:
                if nid == 24:
                    return walk(res_off + (od & 0x7fffffff), level+1)
            return None
        if level == 1:
            # find id 1
            for nid, od in ents:
                if nid == 1:
                    return walk(res_off + (od & 0x7fffffff), level+1)
            return None
        if level == 2:
            # language entries; take first
            nid, od = ents[0]
            doff = res_off + (od & 0x7fffffff)
            size, rva = struct.unpack_from('<II', d, doff)
            data_off = rva_to_off(d, rva, sections)
            return d[data_off:data_off+size]
        return None
    return walk(res_off, 0)

d = open(sys.argv[1], 'rb').read()
data = parse(d)
if not data:
    print("未找到 RT_MANIFEST 资源")
    sys.exit(2)
txt = data.decode('utf-8', 'replace')
print("=== 清单长度", len(data), "字节 ===")
print(txt)
if 'requireAdministrator' in txt:
    print("\n>>> 确认：requestedExecutionLevel = requireAdministrator ✅")
else:
    print("\n>>> 警告：未找到 requireAdministrator")
