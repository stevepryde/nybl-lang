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

{{LEFT}} fixture-left-payload-opener
This third region checks marker-like text in the right-hand payload.
{{MIDDLE}}
{{LEFT}} note-in-right-payload
The active region must survive that payload line.
{{RIGHT}} fixture-right-payload-opener

The default scan excludes this one explicit fixture template. Self-test
expands all regions in memory and passes them through the production region
detector.
