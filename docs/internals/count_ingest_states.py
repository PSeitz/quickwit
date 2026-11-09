#!/usr/bin/env python3
"""Validate and count the documented local graphs. This does not simulate ingestion."""

import json
from collections import defaultdict
from pathlib import Path


def counts(graph):
    states = set(graph["states"]) - set(graph["shared_states"])
    edges = {(start, end) for start, end, _ in graph["edges"] if start != end}
    return len(states), len(edges)


def main():
    directory = Path(__file__).with_name("ingest_states")
    entries = [
        (path.stem, graph)
        for path in sorted(directory.glob("*.json"))
        for graph in json.loads(path.read_text())["graphs"]
    ]
    graphs = {graph["id"]: graph for _, graph in entries}
    assert entries and len(graphs) == len(entries), "missing or duplicate graphs"
    aliases = {
        "running": "shared_runtime_execution.running",
        "ready": "shared_runtime_execution.ready",
        "actor_finalizing": "shared_actor_lifecycle.actor_finalizing",
    }
    subtotals = defaultdict(lambda: [0, 0])
    category_totals = defaultdict(lambda: [0, 0])
    totals = defaultdict(lambda: [0, 0])
    for ledger, graph in entries:
        states = graph["states"]
        shared = graph["shared_states"]
        edges = graph["edges"]
        assert len(states) == len(set(states)), graph["id"]
        assert len(shared) == len(set(shared)) and set(shared) <= set(states)
        assert len(edges) == len({tuple(edge) for edge in edges}), graph["id"]
        assert all(start in states and end in states and trigger for start, end, trigger in edges)
        assert graph["sources"], graph["id"]
        references = aliases | graph.get("shared_state_sources", {})
        for state in shared:
            owner, target = references[state].split(".")
            assert target in graphs[owner]["states"]
            assert target not in graphs[owner]["shared_states"]
            assert set(graph["applies_to"]) <= set(graphs[owner]["applies_to"])
        assert set(graph["applies_to"]) <= {"current", "proposed"}
        for design in graph["applies_to"]:
            for index, value in enumerate(counts(graph)):
                subtotals[design, ledger][index] += value
                category_totals[design, graph["category"]][index] += value
                totals[design][index] += value
    print(f"{'Design / ledger':28} {'States':>8} {'Transitions':>12}")
    for design in sorted(totals):
        for (owner, ledger), values in sorted(subtotals.items()):
            if owner == design:
                print(f"{design + ' / ' + ledger:28} {values[0]:8} {values[1]:12}")
        values = totals[design]
        print(f"{design + ' TOTAL':28} {values[0]:8} {values[1]:12}")
    print("\nCategory             Current states/transitions   Proposed states/transitions")
    for category in sorted({category for _, category in category_totals}):
        current = category_totals.get(("current", category))
        proposed = category_totals.get(("proposed", category))
        current_text = f"{current[0]} / {current[1]}" if current is not None else "—"
        proposed_text = f"{proposed[0]} / {proposed[1]}" if proposed is not None else "—"
        print(f"{category:20} {current_text:>26}   {proposed_text:>27}")
    print("— = no graph in this ledger category, not zero complexity.")


if __name__ == "__main__":
    main()
