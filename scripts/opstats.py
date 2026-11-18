"""Block-type mix of an RPZA stream.

usage: opstats.py WIDTH HEIGHT PACKETS.bin [...]

PACKETS.bin is the output of `ffmpeg -i clip.mov -c:v copy -f rawvideo`:
RPZA packets back to back, each starting with 0xE1 and a 24-bit length.
Prints, per file, what share of blocks each opcode type covers and the
average run length of the multi-block opcodes.
"""
import sys


def stats(data, width, height):
    total_blocks = ((width + 3) // 4) * ((height + 3) // 4)
    blocks = {"skip": 0, "fill": 0, "four": 0, "four_single": 0, "sixteen": 0}
    ops = dict.fromkeys(blocks, 0)
    pos, frames = 0, 0
    while pos + 4 <= len(data):
        size = int.from_bytes(data[pos + 1:pos + 4], "big")
        if data[pos] != 0xE1 or size < 4:
            break
        end, p, left = pos + size, pos + 4, total_blocks
        frames += 1
        while p < end:
            op = data[p]
            p += 1
            n = (op & 0x1F) + 1
            if op & 0x80 == 0:
                p += 1
                if p < end and data[p] & 0x80:
                    kind, n = "four_single", 1
                    p += 2 + 4
                else:
                    kind, n = "sixteen", 1
                    p += 30
            elif op & 0xE0 == 0x80:
                kind = "skip"
            elif op & 0xE0 == 0xA0:
                kind = "fill"
                p += 2
            elif op & 0xE0 == 0xC0:
                kind = "four"
                p += 4 + 4 * min(n, left)
            else:
                break
            n = min(n, left)
            left -= n
            blocks[kind] += n
            ops[kind] += 1
        pos = end
    return frames, blocks, ops


def main():
    width, height = int(sys.argv[1]), int(sys.argv[2])
    for path in sys.argv[3:]:
        frames, blocks, ops = stats(open(path, "rb").read(), width, height)
        total = sum(blocks.values()) or 1
        parts = []
        for kind in blocks:
            share = 100 * blocks[kind] / total
            run = blocks[kind] / ops[kind] if ops[kind] else 0
            parts.append(f"{kind} {share:4.1f}% (run {run:4.1f})")
        covered = 100 * sum(blocks.values()) / ((frames or 1) * ((width + 3) // 4) * ((height + 3) // 4))
        print(f"{frames:4d} frames  " + "  ".join(parts) + f"  [parsed {covered:.1f}% of blocks]")


if __name__ == "__main__":
    main()
