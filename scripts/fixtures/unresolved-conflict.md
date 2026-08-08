# Negative fixture for the conflict-marker guard

{{LEFT}} fixture-left
This tracked documentation fixture represents an unresolved left side.
{{MIDDLE}}
This tracked documentation fixture represents an unresolved right side.
{{RIGHT}} fixture-right

{{LEFT_LONG}} fixture-left-long
This second region uses nine-character markers.
{{MIDDLE_LONG}}
Its purpose is to catch custom merge marker sizes.
{{RIGHT_LONG}} fixture-right-long

The default scan excludes this one explicit fixture template. Self-test
expands both regions in memory and passes them through the production region
detector.
