/** Best-effort tech-stack sniff from common manifests in a project root. */
export declare function sniffStack(root: string): string[];
/** Collection name derived exactly the way Kilo does it. */
export declare function workspaceCollection(workspacePath: string): string;
/** Embed a query via LM Studio. */
export declare function embed(text: string): Promise<number[]>;
export interface CodeHit {
    project: string;
    filePath: string;
    startLine: number;
    endLine: number;
    codeChunk: string;
    score: number;
}
/** Search one Qdrant collection and return the indexed code hits. */
export declare function searchCollection(workspacePath: string, vector: number[], opts?: {
    limit?: number;
    minScore?: number;
    names?: string[];
}): Promise<CodeHit[]>;
/** Optional GPU reranker hook: POST chunk_texts to LM Studio's /rerank if present (else skip). */
export declare function rerank(query: string, docs: Array<{
    id: string;
    text: string;
}>): Promise<Array<{
    id: string;
    score: number;
}>>;
/** Parses a Qdrant /points/query response into {score, payload[]}. Exported for testability. */
export declare function parseQdrantQueryResult(raw: unknown): Array<{
    score: number;
    payload?: Record<string, unknown>;
}>;
/** BM25 ranks `docs` (as {id, text}) against `query` over the given corpus. */
export declare function bm25Rank(query: string, docs: Array<{
    id: string;
    text: string;
}>, k1?: number, b?: number): Array<{
    id: string;
    score: number;
}>;
/**
 * Fuse the dense (Qdrant cosine) ordering with a lexical BM25 ordering over the
 * SAME retrieved candidates using reciprocal-rank fusion — the NAI-Lattice hybrid
 * recall recipe for code. Exact symbol identifiers that dense embeddings miss
 * get rescued by BM25. Cheap, GPU-free, deterministic.
 */
export declare function fuseDenseAndLexical(query: string, dense: Array<CodeHit>): Array<CodeHit>;
