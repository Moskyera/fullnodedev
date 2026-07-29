// Delta review: re-check prior confirmed bugs + scan for new ones.
// Compares against the 15 findings from review-miner-pool (2026-07-24).

let meta = #{
    name: "review-miner-pool-delta",
    description: "Re-verify prior miner/pool bugs and hunt for new ones; produce fixed vs open vs new report",
    when_to_use: "After miner/pool fixes; track regression and remaining open issues",
    phases: [
        #{ title: "Recheck", detail: "adversarially re-verify each prior confirmed bug" },
        #{ title: "Fresh scan", detail: "parallel hunt for NEW bugs not in the prior list" },
        #{ title: "Synthesize", detail: "delta report: fixed / still open / new" },
    ],
};

let status_schema = #{
    "type": "object",
    "required": ["status", "reason", "evidence"],
    "properties": #{
        "status": #{ "type": "string" },
        "reason": #{ "type": "string" },
        "evidence": #{ "type": "string" },
    },
};

let findings_schema = #{
    "type": "object",
    "required": ["findings"],
    "properties": #{
        "findings": #{
            "type": "array",
            "maxItems": 8,
            "items": #{
                "type": "object",
                "required": ["severity", "file", "issue", "impact"],
                "properties": #{
                    "severity": #{ "type": "string" },
                    "file": #{ "type": "string" },
                    "issue": #{ "type": "string" },
                    "impact": #{ "type": "string" },
                },
            },
        },
    },
};

let verdict_schema = #{
    "type": "object",
    "required": ["real", "reason", "evidence"],
    "properties": #{
        "real": #{ "type": "boolean" },
        "reason": #{ "type": "string" },
        "evidence": #{ "type": "string" },
    },
};

let report_schema = #{
    "type": "object",
    "required": ["summary", "fixed_count", "still_open_count", "new_count", "markdown"],
    "properties": #{
        "summary": #{ "type": "string" },
        "fixed_count": #{ "type": "integer" },
        "still_open_count": #{ "type": "integer" },
        "new_count": #{ "type": "integer" },
        "markdown": #{ "type": "string" },
    },
};

let root = "C:/Users/KQHEX/Documents/hacash-fullnodedev";
if args != () && args.root != () {
    root = args.root;
}

log("delta review root=" + root);

// Prior confirmed bugs from review-miner-pool (adversarially verified).
// Each recheck agent must set status to: fixed | still_open | unclear
let prior = [
    #{
        id: "P1",
        severity: "high",
        file: "app/src/block_mining_runtime.rs winners coalesce",
        issue: "Winners coalesced only by height; same-height reorg stale result can out-rank and replace a live valid solution; no epoch/template id on BlockMiningResult; submit without live-template revalidation.",
    },
    #{
        id: "P2",
        severity: "high",
        file: "app/src/poworker.rs template install",
        issue: "Template install only when pending_height > curr_hei or same-height intro_changed; reorg that LOWERs pending height is ignored so workers never job-switch.",
    },
    #{
        id: "P3",
        severity: "high",
        file: "x16rs/opencl/x16rs_diamond.cl reduction",
        issue: "Diamond OpenCL reduction initializes best_hash=0 and leaves best_name uninitialized; scans i=1..; wrong winners vs block kernel seed-from-index pattern.",
    },
    #{
        id: "P4",
        severity: "high",
        file: "x16rs/opencl/x16rs_diamond.cl fences",
        issue: "Diamond kernel uses only CLK_LOCAL_MEM_FENCE while hashes live in global memory; block kernel uses LOCAL|GLOBAL.",
    },
    #{
        id: "P5",
        severity: "high",
        file: "app/src/diaworker.rs diamond submit",
        issue: "push_diamond_mining_success treats any HTTP Ok body as final; non-JSON/missing tx_hash fails permanently without retry; no durable requeue after drain.",
    },
    #{
        id: "P6",
        severity: "medium",
        file: "app/src/poworker.rs LAST_PENDING_INTRO",
        issue: "LAST_PENDING_INTRO overwritten BEFORE set_pending_block_stuff succeeds; failed same-height install never retried.",
    },
    #{
        id: "P7",
        severity: "medium",
        file: "app/src/block_mining_runtime.rs send before job-switch",
        issue: "Job-switch height/epoch checked only AFTER full batch and send(); stale results always enter channel; drain does not filter by live epoch.",
    },
    #{
        id: "P8",
        severity: "medium",
        file: "app/src/block_mining_runtime.rs worker rollover",
        issue: "Worker rollover uses check_hei > mining_hei (advance-only) plus epoch; height decrease without epoch path may not stop workers.",
    },
    #{
        id: "P9",
        severity: "medium",
        file: "x16rs/opencl/sha3_256.cl diamond length",
        issue: "Host builds 61-byte stuff for diamond numbers <= 20000 but sha3_256_hash_diamond always pads as 93-byte; GPU SHA3 diverges from CPU/consensus.",
    },
    #{
        id: "P10",
        severity: "medium",
        file: "app/src/opencl_dia.rs medium hash verify",
        issue: "Diamond GPU success path never recomputes x16rs_hash and byte-compares GPU medium hash (unlike block verify_gpu_best_result).",
    },
    #{
        id: "P11",
        severity: "medium",
        file: "x16rs-cuda batch launch",
        issue: "CUDA batch mining launches with fixed local_size 256 and never calls clamped_block_size; single-hash path clamps.",
    },
    #{
        id: "P12",
        severity: "medium",
        file: "app/src/diaworker.rs submit durability",
        issue: "After MAX_SUBMIT_ATTEMPTS network failure or soft parse failure, drained DiamondMint is dropped with only a log; no local save/requeue.",
    },
    #{
        id: "P13",
        severity: "medium",
        file: "x16rs/opencl/x16rs_diamond.cl unit0",
        issue: "Per-work-item reduction never diamond_hashs unit index 0 into best_name before comparisons.",
    },
    #{
        id: "P14",
        severity: "low",
        file: "app/src/block_mining_runtime.rs total_nonce_space",
        issue: "Drain aggregate adds planned res.nonce_space even when recovery only mined partial window; telemetry misleading.",
    },
    #{
        id: "P15",
        severity: "low",
        file: "app/src/opencl_gpu/block.rs stuff length",
        issue: "OpenCL block upload accepts stuff length <= 512 without enforcing 89-byte block intro that CUDA requires.",
    },
];

// Also re-check consensus items that should already be fixed from earlier sessions
let prior_extra = [
    #{
        id: "C1",
        severity: "critical",
        file: "x16rs/src/diamond.rs",
        issue: "Testnet hack: DMD_L=4 DMD_M=10 instead of mainnet DMD_L=10 DMD_M=16.",
    },
    #{
        id: "C2",
        severity: "critical",
        file: "mint/src/action/diamond_mint.rs",
        issue: "Diamond height%5 rule disabled with if false.",
    },
    #{
        id: "C3",
        severity: "high",
        file: "app/src mining winners/equal target/GPU fatal",
        issue: "Old bugs: only single global best submit; equal-to-target dropped; silent CPU fallback on GPU fail; CUDA no verify; full CPU recovery.",
    },
    #{
        id: "C4",
        severity: "high",
        file: "miner-pool rpc_proxy/stratum",
        issue: "Non-JSON upstream body reported as ret:0 success; stratum used substring ret check.",
    },
];

// --- Phase 1: recheck all prior items ---
phase("Recheck");

let recheck_jobs = [];
let all_prior = [];
for p in prior {
    all_prior.push(p);
}
for p in prior_extra {
    all_prior.push(p);
}

for p in all_prior {
    let rp = "";
    rp += "READ-ONLY recheck of a previously reported bug in " + root + ".\n";
    rp += "Bug id: " + p.id + "\n";
    rp += "Prior severity: " + p.severity + "\n";
    rp += "File/area: " + p.file + "\n";
    rp += "Claimed issue: " + p.issue + "\n\n";
    rp += "Open the relevant source with read_file/grep. Decide status:\n";
    rp += "- fixed: the bug is no longer present; code clearly handles the case (quote the fix).\n";
    rp += "- still_open: the buggy logic is still there (quote the evidence).\n";
    rp += "- unclear: cannot determine (explain what was missing).\n";
    rp += "status MUST be exactly one of: fixed, still_open, unclear.\n";
    rp += "evidence must cite path and what you saw. reason is one short sentence.\n";
    recheck_jobs.push(#{
        prompt: rp,
        label: "recheck:" + p.id,
        capability_mode: "read-only",
        output_schema: status_schema,
    });
}

let recheck_results = parallel(recheck_jobs);

let fixed_list = [];
let open_list = [];
let unclear_list = [];
let ri = 0;
for r in recheck_results {
    let p = all_prior[ri];
    let st = "unclear";
    let reason = "recheck agent failed";
    let evidence = "";
    if r != () && r.success && r.output.status != () {
        st = r.output.status;
        if r.output.reason != () {
            reason = r.output.reason;
        }
        if r.output.evidence != () {
            evidence = r.output.evidence;
        }
    }
    // normalize
    if st != "fixed" && st != "still_open" && st != "unclear" {
        // allow minor variants
        if st == "FIXED" || st == "Fixed" {
            st = "fixed";
        } else if st == "still open" || st == "open" || st == "STILL_OPEN" {
            st = "still_open";
        } else {
            st = "unclear";
        }
    }
    let row = #{
        id: p.id,
        severity: p.severity,
        file: p.file,
        issue: p.issue,
        status: st,
        reason: reason,
        evidence: evidence,
    };
    if st == "fixed" {
        fixed_list.push(row);
    } else if st == "still_open" {
        open_list.push(row);
    } else {
        unclear_list.push(row);
    }
    ri += 1;
}

log("recheck: fixed=" + fixed_list.len().to_string()
    + " open=" + open_list.len().to_string()
    + " unclear=" + unclear_list.len().to_string());

// --- Phase 2: fresh scan for NEW bugs ---
phase("Fresh scan");

let areas = [
    #{
        id: "block-miner",
        paths: "app/src/block_mining_runtime.rs, app/src/mining_batch.rs, app/src/poworker.rs, app/src/mining_runtime.rs, app/src/hash_util.rs",
        focus: "reorg job install, winner epoch tagging, equal target, nonce advance, GPU fail-closed, races",
    },
    #{
        id: "gpu-diamond",
        paths: "x16rs/opencl/x16rs_diamond.cl, x16rs/opencl/x16rs_main.cl, x16rs/opencl/sha3_256.cl, app/src/opencl_dia.rs, app/src/opencl_gpu/, app/src/mining_batch.rs, x16rs-cuda/",
        focus: "diamond/block kernel reduction, fences, SHA3 length, CUDA clamp, integrity verify, buffer sizes",
    },
    #{
        id: "hacd-pool",
        paths: "app/src/diaworker.rs, x16rs/src/diamond.rs, mint/src/action/diamond_mint.rs, mint/src/check/block_build.rs, miner-pool/src/",
        focus: "diamond consensus mainnet, submit durability, pool ret handling, stratum parse",
    },
];

// Build exclusion summary so scanners avoid re-reporting known items
let known_summary = "";
for p in all_prior {
    known_summary += p.id + ": " + p.issue + " | ";
}

let scan_jobs = [];
for a in areas {
    let sp = "";
    sp += "READ-ONLY bug hunt for NEW issues only in " + root + ".\n";
    sp += "Area: " + a.id + "\n";
    sp += "Paths: " + a.paths + "\n";
    sp += "Focus: " + a.focus + "\n\n";
    sp += "ALREADY REPORTED (do NOT re-list these unless you found a DISTINCT new facet):\n";
    sp += known_summary + "\n\n";
    sp += "Use read_file/grep. Prefer real correctness bugs. severity: critical|high|medium|low.\n";
    sp += "Max 8 findings. Empty list valid after inspection. Return findings {severity,file,issue,impact}.\n";
    scan_jobs.push(#{
        prompt: sp,
        label: "scan:" + a.id,
        capability_mode: "read-only",
        output_schema: findings_schema,
    });
}

let scan_results = parallel(scan_jobs);

let new_raw = [];
let si = 0;
for r in scan_results {
    let area = areas[si].id;
    if r != () && r.success && r.output.findings != () {
        for f in r.output.findings {
            new_raw.push(#{
                area: area,
                severity: f.severity,
                file: f.file,
                issue: f.issue,
                impact: f.impact,
            });
        }
    }
    si += 1;
}

log("fresh scan raw new candidates: " + new_raw.len().to_string());

// Verify new candidates (cap 12)
let MAX_NEW_V = 12;
let new_to_v = [];
let ndrop = 0;
for f in new_raw {
    if new_to_v.len() < MAX_NEW_V {
        new_to_v.push(f);
    } else {
        ndrop += 1;
    }
}
if ndrop > 0 {
    log("capped new verify list, dropped " + ndrop.to_string());
}

let new_confirmed = [];
if new_to_v.len() > 0 {
    let vjobs = [];
    for f in new_to_v {
        let vp = "";
        vp += "Adversarially verify this alleged NEW bug under " + root + ".\n";
        vp += "Area: " + f.area + "\nFile: " + f.file + "\nIssue: " + f.issue + "\nImpact: " + f.impact + "\n";
        vp += "Prior known bugs (must not confirm duplicates of these):\n" + known_summary + "\n";
        vp += "Set real=true only if: (1) bug still exists in code, (2) it is NOT the same as a prior known bug, (3) evidence is concrete.\n";
        vp += "Set real=false if fixed, duplicate of known list, wrong, or style-only.\n";
        vjobs.push(#{
            prompt: vp,
            label: "verify-new:" + f.area,
            capability_mode: "read-only",
            output_schema: verdict_schema,
        });
    }
    let vres = parallel(vjobs);
    let vi = 0;
    for v in vres {
        let f = new_to_v[vi];
        if v != () && v.success && v.output.real == true
            && v.output.evidence != () && v.output.evidence != "" {
            new_confirmed.push(#{
                area: f.area,
                severity: f.severity,
                file: f.file,
                issue: f.issue,
                impact: f.impact,
                evidence: v.output.evidence,
                reason: v.output.reason,
            });
        }
        vi += 1;
    }
}

log("new confirmed bugs: " + new_confirmed.len().to_string());

// --- Phase 3: synthesize ---
phase("Synthesize");

let fixed_json = json_encode(fixed_list);
let open_json = json_encode(open_list);
let unclear_json = json_encode(unclear_list);
let new_json = json_encode(new_confirmed);

let yp = "";
yp += "Write a delta bug review report in GitHub-flavored markdown for " + root + ".\n\n";
yp += "FIXED prior bugs (JSON):\n" + fixed_json + "\n\n";
yp += "STILL OPEN prior bugs (JSON):\n" + open_json + "\n\n";
yp += "UNCLEAR prior rechecks (JSON):\n" + unclear_json + "\n\n";
yp += "NEW confirmed bugs (JSON):\n" + new_json + "\n\n";
yp += "Sections required:\n";
yp += "1. Summary (counts: fixed / still open / unclear / new)\n";
yp += "2. Fixed (table id|severity|file|note)\n";
yp += "3. Still open (table id|severity|file|issue)\n";
yp += "4. New bugs (table severity|file|issue|impact)\n";
yp += "5. Unclear (if any)\n";
yp += "6. Recommended next fix order\n";
yp += "Return JSON: summary, fixed_count, still_open_count, new_count, markdown.\n";
yp += "fixed_count=" + fixed_list.len().to_string()
    + " still_open_count=" + open_list.len().to_string()
    + " new_count=" + new_confirmed.len().to_string() + "\n";

let synth = agent(yp, #{
    label: "synthesize-delta",
    capability_mode: "read-only",
    output_schema: report_schema,
});

let md = "# Miner+Pool Delta Review\n\n";
let summary = "Delta review complete.";
let fc = fixed_list.len();
let oc = open_list.len();
let nc = new_confirmed.len();

if synth != () && synth.success && synth.output.markdown != () {
    md = synth.output.markdown;
    if synth.output.summary != () {
        summary = synth.output.summary;
    }
    if synth.output.fixed_count != () {
        fc = synth.output.fixed_count;
    }
    if synth.output.still_open_count != () {
        oc = synth.output.still_open_count;
    }
    if synth.output.new_count != () {
        nc = synth.output.new_count;
    }
} else {
    md += "## Summary\n\nFixed " + fc.to_string() + ", still open " + oc.to_string()
        + ", unclear " + unclear_list.len().to_string()
        + ", new " + nc.to_string() + ".\n\n";
    md += "## Fixed\n\n";
    for x in fixed_list {
        md += "- **" + x.id + "** (" + x.severity + ") `" + x.file + "` — " + x.reason + "\n";
    }
    md += "\n## Still open\n\n";
    for x in open_list {
        md += "- **" + x.id + "** (" + x.severity + ") `" + x.file + "` — " + x.issue + "\n";
    }
    md += "\n## New\n\n";
    for x in new_confirmed {
        md += "- **" + x.severity + "** `" + x.file + "` — " + x.issue + "\n";
    }
    md += "\n## Unclear\n\n";
    for x in unclear_list {
        md += "- **" + x.id + "** — " + x.reason + "\n";
    }
    summary = "Fixed " + fc.to_string() + " / open " + oc.to_string() + " / new " + nc.to_string();
}

let path = write_scratch_file("miner-pool-delta-review.md", md);
log("delta report: " + path);

complete(#{
    summary: summary,
    fixed_count: fc,
    still_open_count: oc,
    unclear_count: unclear_list.len(),
    new_count: nc,
    path: path,
    fixed: fixed_list,
    still_open: open_list,
    unclear: unclear_list,
    new_bugs: new_confirmed,
});
