# Security

Please report vulnerabilities through
[GitHub private vulnerability reporting](https://github.com/wigentic-ai/sbc/security/advisories/new).
Do not open a public issue for an undisclosed vulnerability.

`sbc` delegates authentication to OpenSSH and Docker Sandbox. It never stores
SSH credentials or sandbox secrets. Clipboard images are written only to local
temporary storage and sandbox `/tmp`, then removed after the session ends.
