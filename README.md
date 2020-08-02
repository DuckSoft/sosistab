# Sosistab - an obfuscated datagram transport for horrible networks

Sosistab is an unreliable, obfuscated datagram transport over UDP and/or TCP, designed to achieve high performance even in extremely bad networks. Sosistab can be used for applications like anti-censorship VPNs, reliable communication over radios, game networking, etc.

Features:

- Strong (obfs4-like) obfuscation in both UDP and TCP. Sosistab servers cannot be detected by active probing, and Sosistab traffic is reasonably indistinguishable from random.
- Strong yet lightweight authenticated encryption with ChaCha8/12/20 and 64-bit truncated blake3. ChaCha8 is almost certainly secure enough and blake3 is so fast that we don't need poly1305, which is hard to compose.
- Deniable public-key encryption with triple-x25519. Different clients are
- Congestion-control-defeating TCP mode that tries its best to avoid "TCP-over-TCP" problems for TCP-over-Sosistab-over-TCP
- Autotuning RaptorQ-based error correction that targets a certain application packet loss level
- Avoids last-mile congestive collapse but works around lossy links. Shamelessly unfair in permanently congested WANs --- but that's really their problem, not yours. In any case, permanently congested WANs are observationally identical to lossy links, and any solution for the latter will cause unfairness in the former.
