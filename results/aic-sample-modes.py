import sys, collections
for path, w, h in ((sys.argv[1], 481, 241), (sys.argv[2], 1440, 1080)):
    d = open(path, 'rb').read()
    mbw, mbh = (w + 15) // 16, (h + 15) // 16
    sw, nx = 16, (mbw + 15) // 16
    for i in range(1, mbw):
        if mbw % i == 0 and mbw // i <= 32:
            sw, nx = mbw // i, i
            break
    n = nx * mbh
    off = (24 + 2 * n + 3) & ~3
    modes = collections.Counter()
    sizes = []
    for k in range(n):
        size = int.from_bytes(d[24 + 2 * k:26 + 2 * k], 'little') * 4
        b = int.from_bytes(d[off:off + 2], 'big')
        has_skips, ctype, cbits = b >> 15, b >> 14 & 1, b >> 11 & 7
        stype, sbits = b >> 10 & 1, b >> 7 & 7
        modes[(has_skips, ctype, cbits, stype if has_skips else '-', sbits if has_skips else '-')] += 1
        sizes.append(size)
        off += size
    print(path, 'bytes', len(d), 'quant', d[15], 'byte16 0x%02x' % d[16], 'interlaced', d[16] >> 4 == 3,
          'mb', mbw, mbh, 'slice width', sw, 'slices', n, 'end', off, 'mean slice bytes', sum(sizes) // n)
    print('  luma band (has_skips, coeff_type, coeff_bits, skip_type, skip_bits): count', dict(modes))
