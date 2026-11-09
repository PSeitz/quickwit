#!/usr/bin/env python3
"""Run the paired, finite input-reclamation models. Requires Java and TLC."""

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import tempfile

MODEL = Path(__file__).parent / "ingest_states/tla/InputReclamation.tla"
INVARIANTS = """TypeOK
AcknowledgedRecoverable
PublishedOutputsExist
LocalDeletionAuthorized
RegisteredInputAvailable
PublicationOrder
FeedbackAuthority"""


def config(protocol, units, probe=None):
    text = f"""SPECIFICATION Spec
CONSTANTS
    Protocol = "{protocol}"
    NumUnits = {units}
INVARIANTS
{INVARIANTS}
PROPERTIES
    LocalDeletionRequiresConfirmation
    CompletionStable
CHECK_DEADLOCK FALSE
"""
    if probe:
        text += f"INVARIANT {probe}\n"
    return text


def run(args, directory, name, protocol, units, probe=None, violation=None, model=None,
        violation_kind="Invariant"):
    work = directory / name
    work.mkdir()
    (work / "InputReclamation.tla").write_text(model or MODEL.read_text())
    (work / "InputReclamation.cfg").write_text(config(protocol, units, probe))
    command = [
        "java", "-XX:+UseParallelGC", "-Xmx1g", "-cp", str(args.jar), "tlc2.TLC",
        "-workers", "2", "-fp", str(args.fingerprint), "-noGenerateSpecTE",
        "-metadir", str(work / "states"), "-config", "InputReclamation.cfg",
        "InputReclamation.tla",
    ]
    try:
        result = subprocess.run(command, cwd=work, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=args.timeout)
    except subprocess.TimeoutExpired as error:
        output = error.stdout or ""
        if isinstance(output, bytes):
            output = output.decode(errors="replace")
        (work / "tlc.log").write_text(output)
        raise RuntimeError(f"TLC timed out; inspect {work / 'tlc.log'}") from error
    (work / "tlc.log").write_text(result.stdout)
    if violation:
        expected = f"{violation_kind} {violation} is violated"
        if result.returncode == 0 or expected not in result.stdout:
            raise RuntimeError(f"Expected {expected}; inspect {work / 'tlc.log'}")
        print(f"{name}: expected {violation} witness")
        return None
    if result.returncode != 0 or "No error has been found" not in result.stdout:
        raise RuntimeError(f"TLC failed; inspect {work / 'tlc.log'}")
    counts = re.search(r"(\d+) states generated, (\d+) distinct states found, 0 states left", result.stdout)
    depth = re.search(r"depth of the complete state graph search is (\d+)", result.stdout)
    if not counts or not depth:
        raise RuntimeError(f"Missing completed exploration counts: {work / 'tlc.log'}")
    return int(counts[2]), int(depth[1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=os.environ.get("TLA_JAR"),
                        help="tla2tools.jar path (or set TLA_JAR)")
    parser.add_argument("--units", type=int, nargs="+", default=[1, 2])
    parser.add_argument("--fingerprint", type=int, default=0)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--probes", action="store_true", help="also check reachability witnesses")
    parser.add_argument("--mutations", action="store_true", help="also check temporary broken models")
    args = parser.parse_args()
    if args.jar is None or not args.jar.is_file():
        parser.error("provide --jar or TLA_JAR pointing to tla2tools.jar")
    if any(units < 1 for units in args.units) or len(set(args.units)) != len(args.units):
        parser.error("--units must contain distinct positive integers")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    args.jar = args.jar.resolve()
    directory = Path(tempfile.mkdtemp(prefix="ingest-reclamation-"))
    print(f"Artifacts: {directory}")
    print(f"Model SHA-256: {hashlib.sha256(MODEL.read_bytes()).hexdigest()}")
    print(f"TLC JAR SHA-256: {hashlib.sha256(args.jar.read_bytes()).hexdigest()}")
    print(f"Fingerprint polynomial: {args.fingerprint}; workers: 2; heap: 1 GiB")
    print("| Work units | V2 distinct states | Files distinct states | V2 / files depth |")
    print("|---:|---:|---:|---:|")
    for units in args.units:
        v2 = run(args, directory, f"v2-{units}", "v2", units)
        files = run(args, directory, f"files-{units}", "files", units)
        print(f"| {units} | {v2[0]:,} | {files[0]:,} | {v2[1]} / {files[1]} |", flush=True)

    if args.probes:
        for protocol in ["v2", "files"]:
            run(args, directory, f"complete-{protocol}", protocol, 2,
                "NoFullyReclaimed", "NoFullyReclaimed")
        run(args, directory, "early-local-files", "files", 2,
            "NoLocalReclamationBeforePublication", "NoLocalReclamationBeforePublication")
        run(args, directory, "source-feedback-v2", "v2", 2,
            "NoFeedbackBehindSource", "NoFeedbackBehindSource")
        for probe in ["NoPublicationBeforeRegistrationReply", "NoRemoteDeletionBeforePublicationReply"]:
            run(args, directory, probe, "files", 2, probe, probe)
        for name, protocol, units, invariant in [
            ("no-early-local-v2", "v2", 2, "NoLocalReclamationBeforePublication"),
            ("no-source-feedback-files", "files", 2, "NoFeedbackBehindSource"),
            ("no-source-feedback-one-unit", "v2", 1, "NoFeedbackBehindSource"),
        ]:
            run(args, directory, name, protocol, units, invariant)
            print(f"{name}: invariant holds exhaustively")

    if args.mutations:
        model = MODEL.read_text()
        for name, protocol, before, after, invariant, kind in [
            ("local-delete-after-upload", "files",
             'DeleteLocal ==\n    /\\ handoff.stage = "registered"',
             'DeleteLocal ==\n    /\\ handoff.stage = "uploaded"',
             "LocalDeletionAuthorized", "Invariant"),
            ("local-delete-before-registration-reply", "files",
             'DeleteLocal ==\n    /\\ handoff.stage = "registered"',
             'DeleteLocal ==\n    /\\ handoff.stage = "registrationCommitted"',
             "LocalDeletionRequiresConfirmation", "Action property"),
            ("gossip-polls-publication", "v2",
             '/\\ gossip < emitted /\\ gossip\' = emitted',
             '/\\ gossip < published /\\ gossip\' = published',
             "FeedbackAuthority", "Invariant"),
            ("publication-without-output", "files",
             '/\\ observed + 1 \\in ready', '', "PublishedOutputsExist", "Invariant"),
        ]:
            if model.count(before) != 1:
                raise RuntimeError(f"Mutation {name} must match exactly once")
            run(args, directory, name, protocol, 2, violation=invariant,
                model=model.replace(before, after), violation_kind=kind)


if __name__ == "__main__":
    main()
