# Job Ledger Benchmark Fixture

This repository is intentionally incomplete. The benchmark agent implements a
small durable job-event ledger over several turns while an external verifier
checks behavior that is not embedded in the prompt response.

Run the visible tests with:

```sh
python3 -m unittest discover -s tests -v
```
