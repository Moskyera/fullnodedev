#!/usr/bin/env python3
"""Build kernel trees that vary ONLY the counting sort in X16RS_RUN_REPEAT_LOOP.

    python scripts/x16rs_sort_trees.py x16rs/opencl <outdir> [name ...]

Every tree is a COPY of x16rs/opencl written somewhere else; the shipping tree is
never touched. Unlike scripts/x16rs_gate_trees.py, the trees here are meant to be
CORRECT: the counting sort only decides which work item hashes which slot, and
every slot is still hashed exactly once by its own algorithm, so any valid
permutation gives byte-identical output. `x16rs_gate equiv` must pass on all of
them, and `x16rs_gate ab` reports a ratio that is not confounded by a wrong answer.

Trees:

  stock     verbatim copy. The A leg, and the null control when used as B.

  double    the whole sort runs TWICE per repeat round, both times correctly.
            The second permutation overwrites the first. This is a COST PROBE:
            B/A - 1 is the wall-clock share of one complete counting sort
            (its atomics, its serial scan and its three barriers). No change to
            the sort can win more than that.

  privhist  per work item private histogram. Pass one counts into 16 private
            counters with no atomics at all; then ONE atomic_add per bucket per
            work item claims a disjoint output range (atomic_add returns the old
            value, which is exactly the number of earlier claims); pass two
            scatters with a private cursor and no atomics. LDS atomic operations
            per work item per round fall from 2 * unit_size (384 at unit_size
            192) to 16. The cost is a 16-entry private array indexed by data,
            which AMD may put in scratch.

  wavehist  X16RS_SORT_PARTS private copies of the histogram and the offset
            counters in LDS, selected by (local_id >> 5) & (PARTS - 1), i.e. one
            per wave32 at local_size 256. Same number of atomic operations as
            stock, but each counter is touched by 32 lanes instead of 256. This
            isolates cross-wave address contention from the atomic count itself.

  ballot    subgroup aggregation. One sub_group_ballot per bucket per hash
            collapses a subgroup's 32 increments into a single atomic_add of the
            population count, so the atomic COUNT falls, but sixteen ballots
            replace one DS instruction. Requires cl_khr_subgroup_ballot; the tree
            #errors rather than falling back, so a silent fallback cannot be
            mistaken for "no measurable difference".
"""
import os
import re
import shutil
import sys

# ---------------------------------------------------------------- helpers

def copy_tree(src, dst):
    if os.path.isdir(dst):
        shutil.rmtree(dst)
    os.makedirs(dst)
    for name in os.listdir(src):
        if name.endswith(".cl"):
            shutil.copy(os.path.join(src, name), os.path.join(dst, name))


def write(dst, name, text):
    open(os.path.join(dst, name), "w", encoding="utf-8").write(text)


def replace_once(text, old, new):
    n = text.count(old)
    assert n == 1, "expected 1 occurrence of %r, found %d" % (old[:70], n)
    return text.replace(old, new)


# The sort, exactly as shipped: from the counter reset down to and including the
# barrier that publishes local_order. Everything after it is the dispatch loop.
SORT_START = "        if ((local_id) < 16) { \\\n            (histogram)[(local_id)] = 0; \\\n"
SORT_END = "        barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE); \\\n"


def split_sort(base):
    i = base.index(SORT_START)
    j = base.index(SORT_END, i) + len(SORT_END)
    assert base.count(SORT_START) == 1
    return base[:i], base[i:j], base[j:]


# --------------------------------------------------------------- variants

def build_stock(src, out, base, kernels):
    d = os.path.join(out, "stock")
    copy_tree(src, d)


def build_double(src, out, base, kernels):
    head, sort, tail = split_sort(base)
    d = os.path.join(out, "double")
    copy_tree(src, d)
    write(d, "x16rs.cl", head + sort + sort + tail)


PRIVHIST = """\
        unsigned int privhist[16]; \\
        for (unsigned int i = 0; i < 16; i++) { \\
            privhist[i] = 0; \\
        } \\
        if ((local_id) < 16) { \\
            (histogram)[(local_id)] = 0; \\
        } \\
        for (unsigned int h = 0; h < (unit_size); h++) { \\
            privhist[(local_hashes)[(index) + h].h4[7] % 16]++; \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        for (unsigned int i = 0; i < 16; i++) { \\
            privhist[i] = atomic_add(&(histogram)[i], privhist[i]); \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        if ((local_id) == 0) { \\
            (starting_index)[0] = 0; \\
            for (unsigned char i = 1; i < 16; i++) { \\
                (starting_index)[i] = (starting_index)[i - 1] + (histogram)[i - 1]; \\
            } \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        for (unsigned int h = 0; h < (unit_size); h++) { \\
            unsigned int mod = (local_hashes)[(index) + h].h4[7] % 16; \\
            (local_order)[(starting_index)[mod] + privhist[mod]] = (index) + h; \\
            privhist[mod]++; \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE); \\
"""


def build_quad(src, out, base, kernels):
    """Four sorts per round. Same probe as `double` with three times the signal:
    B/A - 1 should be three times the `double` deficit if the cost is linear,
    which is the check that the probe is measuring the sort and not an artefact."""
    head, sort, tail = split_sort(base)
    d = os.path.join(out, "quad")
    copy_tree(src, d)
    write(d, "x16rs.cl", head + sort * 4 + tail)


def build_privhist(src, out, base, kernels):
    head, sort, tail = split_sort(base)
    d = os.path.join(out, "privhist")
    copy_tree(src, d)
    write(d, "x16rs.cl", head + PRIVHIST + tail)


# PARTS copies of both counter arrays. The zeroing loop is strided so it is
# correct at any local_size, and the partition index is masked so it is correct
# even if local_size is larger than 32 * PARTS.
WAVEHIST = """\
        for (unsigned int i = (local_id); i < 16 * X16RS_SORT_PARTS; i += (local_size)) { \\
            (histogram)[i] = 0; \\
            (offset)[i] = 0; \\
        } \\
        const unsigned int sort_part = ((local_id) >> 5) & (X16RS_SORT_PARTS - 1); \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        for (unsigned int h = 0; h < (unit_size); h++) { \\
            unsigned int mod = (local_hashes)[(index) + h].h4[7] % 16; \\
            atomic_inc(&(histogram)[sort_part * 16 + mod]); \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        if ((local_id) < 16) { \\
            unsigned int run = 0; \\
            for (unsigned int p = 0; p < X16RS_SORT_PARTS; p++) { \\
                unsigned int c = (histogram)[p * 16 + (local_id)]; \\
                (histogram)[p * 16 + (local_id)] = run; \\
                run += c; \\
            } \\
            (starting_index)[(local_id)] = run; \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        if ((local_id) == 0) { \\
            unsigned int run = 0; \\
            for (unsigned char i = 0; i < 16; i++) { \\
                unsigned int c = (starting_index)[i]; \\
                (starting_index)[i] = run; \\
                run += c; \\
            } \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        for (unsigned int h = 0; h < (unit_size); h++) { \\
            unsigned int mod = (local_hashes)[(index) + h].h4[7] % 16; \\
            unsigned int pos = (starting_index)[mod] \\
                + (histogram)[sort_part * 16 + mod] \\
                + atomic_inc(&(offset)[sort_part * 16 + mod]); \\
            (local_order)[pos] = (index) + h; \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE); \\
"""


def build_wavehist(src, out, base, kernels):
    head, sort, tail = split_sort(base)
    d = os.path.join(out, "wavehist")
    copy_tree(src, d)
    text = "#define X16RS_SORT_PARTS 8\n" + head + WAVEHIST + tail
    write(d, "x16rs.cl", text)
    for name, src_text in kernels.items():
        t = replace_once(src_text,
                         "__local unsigned int ALIGN histogram[16];",
                         "__local unsigned int ALIGN histogram[16 * X16RS_SORT_PARTS];")
        t = replace_once(t,
                         "__local unsigned int ALIGN offset[16];",
                         "__local unsigned int ALIGN offset[16 * X16RS_SORT_PARTS];")
        write(d, name, t)


BALLOT = """\
        if ((local_id) < 16) { \\
            (histogram)[(local_id)] = 0; \\
            (offset)[(local_id)] = 0; \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        for (unsigned int h = 0; h < (unit_size); h++) { \\
            unsigned int mod = (local_hashes)[(index) + h].h4[7] % 16; \\
            for (unsigned int m = 0; m < 16; m++) { \\
                uint4 vote = sub_group_ballot(mod == m); \\
                unsigned int cnt = sub_group_ballot_bit_count(vote); \\
                if (cnt != 0 && get_sub_group_local_id() == sub_group_ballot_find_lsb(vote)) { \\
                    atomic_add(&(histogram)[m], cnt); \\
                } \\
            } \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        if ((local_id) == 0) { \\
            (starting_index)[0] = 0; \\
            for (unsigned char i = 1; i < 16; i++) { \\
                (starting_index)[i] = (starting_index)[i - 1] + (histogram)[i - 1]; \\
            } \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE); \\
        for (unsigned int h = 0; h < (unit_size); h++) { \\
            unsigned int mod = (local_hashes)[(index) + h].h4[7] % 16; \\
            unsigned int pos = 0; \\
            for (unsigned int m = 0; m < 16; m++) { \\
                uint4 vote = sub_group_ballot(mod == m); \\
                unsigned int cnt = sub_group_ballot_bit_count(vote); \\
                if (cnt == 0) { \\
                    continue; \\
                } \\
                unsigned int lead = sub_group_ballot_find_lsb(vote); \\
                unsigned int claimed = 0; \\
                if (get_sub_group_local_id() == lead) { \\
                    claimed = atomic_add(&(offset)[m], cnt); \\
                } \\
                claimed = sub_group_broadcast(claimed, lead); \\
                if (mod == m) { \\
                    pos = (starting_index)[m] + claimed + sub_group_ballot_exclusive_scan(vote); \\
                } \\
            } \\
            (local_order)[pos] = (index) + h; \\
        } \\
        barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE); \\
"""

BALLOT_PROLOGUE = """\
#pragma OPENCL EXTENSION cl_khr_subgroups : enable
#pragma OPENCL EXTENSION cl_khr_subgroup_ballot : enable
"""


def build_ballot(src, out, base, kernels):
    head, sort, tail = split_sort(base)
    d = os.path.join(out, "ballot")
    copy_tree(src, d)
    write(d, "x16rs.cl", BALLOT_PROLOGUE + head + BALLOT + tail)


# The shipped scan is 16 dependent read-modify-writes done by work item 0 while
# the other 255 wait on a barrier. This does the same 16 exclusive sums on 16
# work items at once: every lane reads only, nothing is carried between lanes, so
# the dependent chain of 16 LDS round trips becomes one masked loop in one wave.
FASTSCAN_OLD = """\
        if ((local_id) == 0) { \\
            (starting_index)[0] = 0; \\
            for (unsigned char i = 1; i < 16; i++) { \\
                (starting_index)[i] = (starting_index)[i - 1] + (histogram)[i - 1]; \\
            } \\
        } \\
"""

FASTSCAN_NEW = """\
        if ((local_id) < 16) { \\
            unsigned int scan_sum = 0; \\
            for (unsigned int i = 0; i < (local_id); i++) { \\
                scan_sum += (histogram)[i]; \\
            } \\
            (starting_index)[(local_id)] = scan_sum; \\
        } \\
"""


def build_fastscan(src, out, base, kernels):
    d = os.path.join(out, "fastscan")
    copy_tree(src, d)
    write(d, "x16rs.cl", replace_once(base, FASTSCAN_OLD, FASTSCAN_NEW))


BUILDERS = {
    "stock": build_stock,
    "fastscan": build_fastscan,
    "double": build_double,
    "quad": build_quad,
    "privhist": build_privhist,
    "wavehist": build_wavehist,
    "ballot": build_ballot,
}


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    src, out = sys.argv[1], sys.argv[2]
    names = sys.argv[3:] or list(BUILDERS)
    base = open(os.path.join(src, "x16rs.cl"), encoding="utf-8").read()
    kernels = {
        name: open(os.path.join(src, name), encoding="utf-8").read()
        for name in ("x16rs_main.cl", "x16rs_diamond.cl")
    }
    os.makedirs(out, exist_ok=True)
    for name in names:
        BUILDERS[name](src, out, base, kernels)
        print("built", os.path.join(out, name))
    return 0


if __name__ == "__main__":
    sys.exit(main())
