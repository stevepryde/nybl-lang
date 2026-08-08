# Negative fixture for the conflict-marker guard

{{LEFT}} fixture-left
This tracked documentation fixture represents an unresolved left side.
{{MIDDLE}}
This tracked documentation fixture represents an unresolved right side.
{{RIGHT}} fixture-right

The default scan excludes this one explicit fixture template. Running
`scripts/check-conflict-markers.sh --self-test` expands its placeholders to
real markers in memory and must detect them.
