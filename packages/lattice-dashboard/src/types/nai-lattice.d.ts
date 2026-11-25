import type { NaiLatticeSdkV1 } from '../sdk/module-sdk';

declare global {
  interface Window {
    /** Same-origin module SDK v1; installed by the Lattice shell. */
    naiLattice?: NaiLatticeSdkV1;
  }
}

export {};
