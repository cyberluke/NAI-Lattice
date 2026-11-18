import {readFileSync} from 'node:fs'
import {createHash} from 'node:crypto'

// Direct company authority lookup, independent of code indexing and embeddings.
// The path is host configuration, never an arbitrary tool-supplied file path.
export function readCompanyPolicy() {
  const path=process.env.NAI_AAF_DOCUMENT ?? '<workspace>/NAI-OS/docs/AAF.md'
  const bytes=readFileSync(path)
  if(bytes.length>128*1024)throw new Error('Company policy exceeds the bounded document size')
  return {id:'nai.aaf',name:'Ambient Asset Federation',path,sha256:createHash('sha256').update(bytes).digest('hex'),
    document:bytes.toString('utf8'),authority:'Canonical company doctrine; read the full document, not only the delta shorthand',
    next:'Use Foundry foundry_aaf_discover and foundry_aaf_decide to record source-backed reuse decisions. BLUEPRINT execution/package operations extend this doctrine; they do not replace it.'}
}
