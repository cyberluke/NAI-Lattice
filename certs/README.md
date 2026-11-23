# Public trust certificates

The NAI sovereign root, intermediate, and product-leaf certificates are
distributed with each signed release and through the private trust index.
They are intentionally not stored in this source repository.

The private keys are never committed. The release box stages the public
certificates in `.cert/` for signing and in the release artifact set.
