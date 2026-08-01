#!/usr/bin/env python3
"""Build the modified kernel trees the x16rs gate is exercised with.

Every tree is a COPY of x16rs/opencl written somewhere else. The shipping tree is
never touched, and `app/src/opencl_gpu/compile.rs` fingerprints the full contents
of every .cl file, so each copy recompiles from source instead of picking up a
cached binary.

Every tree here produces WRONG hashes on purpose. They are measuring and
self-testing instruments. Never point a miner at one.

    python scripts/x16rs_gate_trees.py x16rs/opencl <outdir> [faults|forced|subst|all]

faults   three deliberate defects, used to prove the gate can fail:
           faults/A  shabal (algo 13) counter Wlow 1 -> 2      one algorithm, always wrong
           faults/B  the barrier at x16rs.cl:269 deleted        a race, not one algorithm
           faults/C  one bit flipped in blake's IV              a single-bit constant

BOTH BACKENDS. These trees drive the CUDA gate as well as the OpenCL one, because
x16rs-cuda/cuda/block_miner.cu #includes util.cl, sha3_256.cl and x16rs.cl out of
the same directory. OpenCL takes the tree at runtime (`--opencl-dir`); CUDA
compiles at build time, so it takes it through an environment variable and a
rebuild:

    X16RS_CUDA_KERNEL_DIR=<outdir>/faults/A \\
      cargo build --release --features cuda --bin x16rs_gate
    x16rs_gate equiv --backend cuda        # must exit 3

For that to mean anything every constant the kernels use has to live in these
.cl files and nowhere else. block_miner.cu used to carry its own copy of blake's
IV, which made faults/C compile to byte-identical CUDA PTX - a broken tree the
CUDA gate would have passed. `x16rs.cl` now exports X16RS_H_BLAKE_INIT and the
.cu reads it; `x16rs-cuda`'s test suite fails if the duplicate returns. Add a
fault that patches a constant, and check it actually changes both backends.

forced   forced/algoNN, NN = 00..15: every round runs algorithm NN. Isolates one
         algorithm's cost, but the compiler drops the other fifteen branches, so
         register pressure and occupancy are NOT production-like.

subst    subst/subNN: all sixteen branches stay, only `case NN:` calls shabal
         instead of algorithm NN. Register pressure stays close to production.
         subst/allshabal has every branch calling shabal, which pins the floor.
         subst/sub13 is the null control: shabal substituted by shabal must
         measure 1.000.
"""
import os
import re
import shutil
import sys


def copy_tree(src, dst):
    if os.path.isdir(dst):
        shutil.rmtree(dst)
    os.makedirs(dst)
    for name in os.listdir(src):
        if name.endswith(".cl"):
            shutil.copy(os.path.join(src, name), os.path.join(dst, name))


def patch(dst, text):
    open(os.path.join(dst, "x16rs.cl"), "w", encoding="utf-8").write(text)


def replace_once(text, old, new):
    n = text.count(old)
    assert n == 1, "expected 1 occurrence of %r, found %d" % (old[:60], n)
    return text.replace(old, new)


ARM = re.compile(
    r"(                case (\d+): \\\n                    )"
    r"hash_x16rs_func_\d+\(&\(local_hashes\)\[hash_pos\[0\]\][^;]*\);"
)
SHABAL_CALL = "hash_x16rs_func_13(&(local_hashes)[hash_pos[0]]);"

SELECTORS = [
    ("            unsigned char mod = (local_hashes)[(index) + h].h4[7] % 16; \\",
     "            unsigned char mod = %d; \\"),
    ("            unsigned int mod = (local_hashes)[(index) + h].h4[7] % 16; \\",
     "            unsigned int mod = %d; \\"),
    ("            switch ((local_hashes)[hash_pos[0]].h4[7] % 16) { \\",
     "            switch (%d) { \\"),
]

BARRIER = ("            } \\\n"
           "            barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE); \\\n"
           "        } \\\n    }")


def build_faults(src, out, base):
    a = replace_once(base,
                     "  sph_u32 Wlow = 1, Whigh = 0;\n\n  INPUT_BLOCK_ADD;",
                     "  sph_u32 Wlow = 2, Whigh = 0;\n\n  INPUT_BLOCK_ADD;")
    b = replace_once(base, BARRIER, "            } \\\n        } \\\n    }")
    c = base.replace("SPH_C64(0x6A09E667F3BCC908)",
                     "SPH_C64(0x6A09E667F3BCC909)", 1)
    assert c != base
    for name, text in (("A", a), ("B", b), ("C", c)):
        d = os.path.join(out, "faults", name)
        copy_tree(src, d)
        patch(d, text)
    print("faults A (shabal Wlow), B (barrier removed), C (blake IV bit) ->",
          os.path.join(out, "faults"))


def build_forced(src, out, base):
    for old, _ in SELECTORS:
        assert base.count(old) == 1, old[:60]
    for algo in range(16):
        d = os.path.join(out, "forced", "algo%02d" % algo)
        copy_tree(src, d)
        text = base
        for old, new in SELECTORS:
            text = text.replace(old, new % algo)
        patch(d, text)
    print("16 forced-algorithm trees ->", os.path.join(out, "forced"))


def build_subst(src, out, base):
    assert len(ARM.findall(base)) == 16, "switch arms not found"

    def make(name, targets):
        d = os.path.join(out, "subst", name)
        copy_tree(src, d)
        text, n = ARM.subn(
            lambda m: m.group(1) + SHABAL_CALL
            if int(m.group(2)) in targets else m.group(0),
            base)
        assert n == 16
        patch(d, text)

    for algo in range(16):
        make("sub%02d" % algo, {algo})
    make("allshabal", set(range(16)))
    print("16 substitution trees + allshabal ->", os.path.join(out, "subst"))


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    src, out = sys.argv[1], sys.argv[2]
    which = sys.argv[3] if len(sys.argv) > 3 else "all"
    base = open(os.path.join(src, "x16rs.cl"), encoding="utf-8").read()
    os.makedirs(out, exist_ok=True)
    if which in ("faults", "all"):
        build_faults(src, out, base)
    if which in ("forced", "all"):
        build_forced(src, out, base)
    if which in ("subst", "all"):
        build_subst(src, out, base)
    return 0


if __name__ == "__main__":
    sys.exit(main())
