import type { InputAnswer, InputContract } from 'shared/types';

export type InputBridge = {
  id: string;
  inputId: string;
  version: number;
  loads: number;
  valid: boolean;
};

export function createInputBridge(
  inputId: string,
  version: number
): InputBridge {
  return { id: crypto.randomUUID(), inputId, version, loads: 0, valid: true };
}

export function inputFrameLoaded(bridge: InputBridge): boolean {
  bridge.loads += 1;
  if (bridge.loads > 1) bridge.valid = false;
  return bridge.valid;
}

export function validateInputAnswer(
  answer: unknown,
  contract: InputContract
): answer is InputAnswer {
  if (!answer || typeof answer !== 'object' || Array.isArray(answer))
    return false;
  const value = answer as Record<string, unknown>;
  if (Object.keys(value).some((key) => !['choice_id', 'text'].includes(key)))
    return false;
  if (
    value.choice_id != null &&
    (typeof value.choice_id !== 'string' ||
      !contract.choices.some((choice) => choice.id === value.choice_id))
  )
    return false;
  if (
    value.text != null &&
    (typeof value.text !== 'string' ||
      (value.text !== '' && !contract.allow_text))
  )
    return false;
  if (new TextEncoder().encode(JSON.stringify(value)).length > 256 * 1024)
    return false;
  return (
    value.choice_id != null ||
    (typeof value.text === 'string' && value.text.trim() !== '') ||
    contract.allow_no_answer_yet
  );
}

export function readInputProposal(
  event: Pick<MessageEvent, 'source' | 'data'>,
  frameWindow: Window | null,
  bridge: InputBridge,
  contract: InputContract
): InputAnswer | null {
  if (!bridge.valid || !frameWindow || event.source !== frameWindow)
    return null;
  const data = event.data;
  if (
    !data ||
    data.type !== 'bc.input.propose' ||
    data.protocol !== 1 ||
    data.bridge_id !== bridge.id ||
    data.input_id !== bridge.inputId ||
    data.version !== bridge.version ||
    !validateInputAnswer(data.answer, contract)
  )
    return null;
  return {
    choice_id: data.answer.choice_id ?? null,
    text: data.answer.text ?? null,
  };
}

// The trusted embedding document applies frame-src to the inner frame's navigations.
// The application shell also contains terminals and previews with separate policies.
export function inputEmbeddingDocument(
  contentUrl: string,
  bridge: InputBridge,
  contentHash: string
): string {
  const url = new URL(contentUrl, window.location.href);
  if (!['http:', 'https:'].includes(url.protocol))
    throw new Error('Invalid Input content URL');
  const path =
    /^\/api\/(?:host\/[^/]+\/)?workspaces\/[^/]+\/inputs\/[^/]+\/versions\/\d+\/content$/;
  const relayPath =
    /^\/v1\/relay\/h\/[^/]+\/s\/[^/]+\/api\/workspaces\/[^/]+\/inputs\/[^/]+\/versions\/\d+\/content$/;
  if (!path.test(url.pathname) && !relayPath.test(url.pathname))
    throw new Error('Invalid Input content route');
  const nonce = crypto.randomUUID();
  const config = JSON.stringify({
    url: url.href,
    input_id: bridge.inputId,
    version: bridge.version,
    bridge_id: bridge.id,
    content_hash: contentHash,
  }).replace(/</g, '\\u003c');
  const frameSource = `${url.origin}${url.pathname}`;
  return `<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'nonce-${nonce}'; style-src 'unsafe-inline'; frame-src ${frameSource}; connect-src 'none'; base-uri 'none'; form-action 'none'"><meta name="referrer" content="no-referrer"><style>html,body,iframe{margin:0;width:100%;height:100%;border:0;display:block}body{background:white}</style></head><body><script nonce="${nonce}">
const config=${config};
const frame=document.createElement('iframe');
frame.sandbox='allow-scripts'; frame.title='Input page'; frame.referrerPolicy='no-referrer';
let loads=0, valid=true;
frame.addEventListener('load',()=>{
  loads++;
  if(loads>1){ valid=false; parent.postMessage({type:'bc.input.navigated',bridge_id:config.bridge_id},'*'); frame.remove(); return; }
  frame.contentWindow.postMessage({type:'bc.input.init',protocol:1,input_id:config.input_id,version:config.version,content_hash:config.content_hash,bridge_id:config.bridge_id},'*');
  parent.postMessage({type:'bc.input.loaded',bridge_id:config.bridge_id},'*');
});
window.addEventListener('message',event=>{
  if(!valid || event.source!==frame.contentWindow) return;
  const d=event.data;
  if(!d || d.type!=='bc.input.propose' || d.protocol!==1 || d.bridge_id!==config.bridge_id || d.input_id!==config.input_id || d.version!==config.version) return;
  try{if(JSON.stringify(d).length>262144)return;}catch{return;}
  parent.postMessage(d,'*');
});
frame.src=config.url; document.body.append(frame);
</script></body></html>`;
}
