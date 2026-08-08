# Positive fixture for the conflict-marker guard

This Markdown uses a valid Setext heading. Its seven-character underline is
not a conflict without a preceding opening marker and later closing marker.

Legitimate heading
{{SETEXT}}

Self-test expands the placeholder to a seven-character Setext underline in
memory so Git's own conflict-marker check does not reject the test fixture.
