// Multi-agent bug review of the Hacash miner + pool stack.
// Parallel dimension reviewers → adversarial verify → synthesis report.

let meta = #{
    name: "review-miner-pool",
    description: "Parallel bug review of miner (poworker/diaworker/OpenCL/CUDA) and pool (hac-pool/stratum), with adversarial verification",
    when_to_use: "After miner or pool changes; pre-release audit of mining correctness and consensus",
    phases: [
        #{ title: "Review", detail: "one reviewer per area of the miner/pool stack" },
        #{ title: "Verify", detail: "adversarial check of each claimed finding" },
        #{ title: "Synthesize", detail: "single ranked bug report" },
    ],
};

let findings_schema = #{
    "type": "object",
    "required": ["findings"],
    "properties": #{
        "findings": #{
            "type": "array",
            "maxItems": 10,
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
    "required": ["summary", "confirmed_count", "open_count", "markdown"],
    "properties": #{
        "summary": #{ "type": "string" },
        "confirmed_count": #{ "type": "integer" },
        "open_count": #{ "type": "integer" },
        "markdown": #{ "type": "string" },
    },
};

// --- args ---
let root = "C:/Users/KQHEX/Documents/hacash-fullnodedev";
if args != () && args.root != () {
    root = args.root;
}
let max_findings = 8;
if args != () && args.max_findings != () {
    max_findings = args.max_findings;
}

log("review-miner-pool root=" + root);

// --- Phase 1: parallel area reviews (read-only) ---
phase("Review");

let areas = [
    #{
        id: "block-miner",
        paths: "app/src/block_mining_runtime.rs, app/src/mining_batch.rs, app/src/poworker.rs, app/src/hash_util.rs, app/src/mining_runtime.rs",
        focus: "block PoW: job switch, nonce advance, winner submit (all heights), equal-inclusive target, GPU init fail-closed, hashrate accounting, race conditions",
    },
    #{
        id: "gpu-backends",
        paths: "app/src/opencl_gpu/, app/src/mining_batch.rs, app/src/cuda_pow.rs, app/src/opencl_dia.rs, x16rs/opencl/, x16rs-cuda/",
        focus: "OpenCL/CUDA integrity verify, buffer OOB, bounded recovery vs full CPU fallback, custom_nonce gating for diamonds, kernel/host mismatch",
    },
    #{
        id: "diamond-consensus",
        paths: "x16rs/src/diamond.rs, app/src/diaworker.rs, mint/src/action/diamond_mint.rs, mint/src/check/block_build.rs, mint/src/check/block_accept.rs, mint/src/api/",
        focus: "mainnet DMD_L=10 DMD_M=16, height%5 diamond rule, name extraction via check_diamond_hash_result (not hardcoded slices), submit retries, testnet hacks left behind",
    },
    #{
        id: "pool",
        paths: "miner-pool/src/ (main, rpc_proxy, stratum, upstream, job, config), pool-spike/src/ if present",
        focus: "submit success/failure semantics (non-JSON must not be ret:0), stratum ret parsing, auth token, share attribution, stale job handling",
    },
];

let review_jobs = [];
for a in areas {
    let p = "";
    p += "You are a senior mining-protocol code reviewer doing a READ-ONLY bug hunt.\n";
    p += "Repository root: " + root + "\n";
    p += "Area id: " + a.id + "\n";
    p += "Focus: " + a.focus + "\n";
    p += "Primary paths (relative to root): " + a.paths + "\n\n";
    p += "REQUIRED process:\n";
    p += "1. Use read_file and grep on the real files under the root. Do NOT invent findings from memory.\n";
    p += "2. Prefer REAL correctness/security bugs over style nits.\n";
    p += "3. For each finding set severity to one of: critical, high, medium, low.\n";
    p += "4. file must be a concrete path like app/src/foo.rs:LINE or path without line if range.\n";
    p += "5. At most " + max_findings.to_string() + " findings. Empty findings is valid ONLY after you inspected the paths.\n";
    p += "6. If code already has an explicit fix comment for a historical bug, do not re-report it as open unless the bug still exists.\n";
    p += "7. Return JSON matching the schema: findings array of {severity, file, issue, impact}.\n";
    review_jobs.push(#{
        prompt: p,
        label: "review:" + a.id,
        capability_mode: "read-only",
        output_schema: findings_schema,
    });
}

let review_results = parallel(review_jobs);

let all_findings = [];
let area_i = 0;
for r in review_results {
    let area_id = areas[area_i].id;
    if r == () || !r.success {
        log("review panel failed or empty for " + area_id);
    } else if r.output.findings != () {
        for f in r.output.findings {
            // Tag area for later synthesis
            let item = #{
                area: area_id,
                severity: f.severity,
                file: f.file,
                issue: f.issue,
                impact: f.impact,
            };
            all_findings.push(item);
        }
    }
    area_i += 1;
}

log("raw findings collected: " + all_findings.len().to_string());

if all_findings.len() == 0 {
    let empty_md = "# Miner + Pool Review\n\nNo findings after parallel area reviews (block-miner, gpu-backends, diamond-consensus, pool).\n";
    let path = write_scratch_file("miner-pool-review.md", empty_md);
    complete(#{
        summary: "No findings from area reviewers.",
        confirmed_count: 0,
        open_count: 0,
        path: path,
        report: empty_md,
    });
}

// Cap verification fan-out to keep budget headroom for synthesis
let MAX_VERIFY = 16;
let to_verify = [];
let dropped = 0;
let fi = 0;
for f in all_findings {
    if to_verify.len() < MAX_VERIFY {
        to_verify.push(f);
    } else {
        dropped += 1;
    }
    fi += 1;
}
if dropped > 0 {
    log("capped verify list: dropped " + dropped.to_string() + " extra raw findings");
}

// --- Phase 2: adversarial verify ---
phase("Verify");

let vjobs = [];
for f in to_verify {
    let vp = "";
    vp += "Adversarially verify this alleged bug against the REAL code under " + root + ".\n";
    vp += "Claimed area: " + f.area + "\n";
    vp += "File: " + f.file + "\n";
    vp += "Issue: " + f.issue + "\n";
    vp += "Impact claimed: " + f.impact + "\n";
    vp += "Severity claimed: " + f.severity + "\n\n";
    vp += "You MUST open the file(s) with read_file/grep and check whether the bug still exists.\n";
    vp += "Set real=true ONLY if you independently confirm the bug is still present with concrete evidence (quote or describe the exact logic).\n";
    vp += "Set real=false if the code already fixes it, the claim is wrong, speculative, or style-only.\n";
    vp += "evidence must cite path and what you saw. reason is a short verdict sentence.\n";
    vjobs.push(#{
        prompt: vp,
        label: "verify:" + f.area,
        capability_mode: "read-only",
        output_schema: verdict_schema,
    });
}

let verdicts = parallel(vjobs);

let confirmed = [];
let rejected = [];
let vi = 0;
for v in verdicts {
    let f = to_verify[vi];
    if v != () && v.success && v.output.real == true
        && v.output.evidence != () && v.output.evidence != "" {
        confirmed.push(#{
            area: f.area,
            severity: f.severity,
            file: f.file,
            issue: f.issue,
            impact: f.impact,
            evidence: v.output.evidence,
            reason: v.output.reason,
        });
    } else {
        let why = "unverified or not real";
        if v != () && v.success && v.output.reason != () {
            why = v.output.reason;
        }
        rejected.push(#{
            area: f.area,
            file: f.file,
            issue: f.issue,
            why: why,
        });
    }
    vi += 1;
}

log("confirmed " + confirmed.len().to_string() + " / verified " + to_verify.len().to_string());

// --- Phase 3: synthesize markdown report ---
phase("Synthesize");

let conf_json = json_encode(confirmed);
let rej_json = json_encode(rejected);

let sp = "";
sp += "Synthesize a final miner+pool bug review report in GitHub-flavored markdown.\n";
sp += "Repository: " + root + "\n\n";
sp += "CONFIRMED findings (JSON, already adversarially verified — treat as open bugs):\n";
sp += conf_json + "\n\n";
sp += "REJECTED / unverified claims (JSON — do not list as open bugs; may mention briefly as closed):\n";
sp += rej_json + "\n\n";
sp += "Write markdown with sections:\n";
sp += "1. Summary (2-4 sentences)\n";
sp += "2. Confirmed open bugs table: severity | file | issue | impact\n";
sp += "3. Notes on already-fixed / rejected claims (short)\n";
sp += "4. Recommended fix order\n";
sp += "Return JSON: summary, confirmed_count, open_count (same as confirmed), markdown (full report body).\n";
sp += "confirmed_count and open_count must equal " + confirmed.len().to_string() + ".\n";

let synth = agent(sp, #{
    label: "synthesize-report",
    capability_mode: "read-only",
    output_schema: report_schema,
});

let md = "# Miner + Pool Review\n\n";
let summary = "Review completed.";
let conf_n = confirmed.len();
let open_n = confirmed.len();

if synth != () && synth.success && synth.output.markdown != () {
    md = synth.output.markdown;
    if synth.output.summary != () {
        summary = synth.output.summary;
    }
    if synth.output.confirmed_count != () {
        conf_n = synth.output.confirmed_count;
    }
    if synth.output.open_count != () {
        open_n = synth.output.open_count;
    }
} else {
    // Fallback local markdown if synthesis agent fails
    md += "## Summary\n\n";
    md += "Confirmed " + confirmed.len().to_string() + " findings after adversarial verification.\n\n";
    md += "## Confirmed open bugs\n\n";
    for c in confirmed {
        md += "- **" + c.severity + "** `" + c.file + "` — " + c.issue + " (impact: " + c.impact + ")\n";
    }
    md += "\n## Rejected\n\n";
    for r in rejected {
        md += "- `" + r.file + "` — " + r.issue + " — " + r.why + "\n";
    }
    summary = "Confirmed " + confirmed.len().to_string() + " open bugs (fallback report).";
}

let path = write_scratch_file("miner-pool-review.md", md);
log("report written: " + path);

complete(#{
    summary: summary,
    confirmed_count: conf_n,
    open_count: open_n,
    path: path,
    confirmed: confirmed,
    rejected_count: rejected.len(),
});
