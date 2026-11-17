// 此函数在目标页面执行；参数由 JSON 序列化，网页内容不能变成宿主代码。
export function pageOperation(action, args) {
  const roots = [];
  function visit(root, offsetX = 0, offsetY = 0) {
    roots.push({ root, offsetX, offsetY });
    for (const element of root.querySelectorAll('*')) {
      if (element.shadowRoot) visit(element.shadowRoot, offsetX, offsetY);
      if (element.tagName === 'IFRAME') {
        try {
          const rect = element.getBoundingClientRect();
          if (element.contentDocument) visit(element.contentDocument, offsetX + rect.left, offsetY + rect.top);
        } catch { /* 跨源 iframe 不伪装为可读取，返回限制由帮助说明。 */ }
      }
    }
  }
  visit(document);
  function selectorFor(element) {
    if (element.id) return `#${CSS.escape(element.id)}`;
    const parts = [];
    for (let node = element; node?.nodeType === 1; node = node.parentElement) {
      const siblings = node.parentElement ? [...node.parentElement.children].filter(x => x.tagName === node.tagName) : [node];
      parts.unshift(`${node.tagName.toLowerCase()}:nth-of-type(${siblings.indexOf(node) + 1})`);
    }
    return parts.join(' > ');
  }
  if (action === 'snapshot') {
    const elements = [];
    for (const { root } of roots) {
      for (const element of root.querySelectorAll('a,button,input,textarea,select,[role="button"],[contenteditable="true"]')) {
        if (!element.getClientRects().length) continue;
        elements.push({ selector: selectorFor(element), tag: element.tagName.toLowerCase(),
          name: (element.getAttribute('aria-label') || element.innerText || element.getAttribute('placeholder') || '').slice(0,300),
          value: element.type === 'password' ? '[password]' : String(element.value || '').slice(0,300) });
        if (elements.length >= 150) break;
      }
      if (elements.length >= 150) break;
    }
    return { title: document.title, url: location.href, ready_state:document.readyState, text: (document.body?.innerText || '').slice(0,16000), elements,
      limitation: 'Use frames and frame_id for cross-origin frames. Observe again after changes.' };
  }
  if (typeof args.selector !== 'string' || !args.selector) throw Error('selector is required');
  const matches = roots.flatMap(context => [...context.root.querySelectorAll(args.selector)]
    .filter(element => element.getClientRects().length).map(element => ({...context, element})));
  if (matches.length !== 1) throw Error(`Expected one visible match, found ${matches.length}`);
  const { element } = matches[0];
  if (element.disabled || element.readOnly && action === 'fill') throw Error('Target is disabled or read-only');
  element.scrollIntoView({ block: 'center', inline: 'center' });
  if (action === 'click') {
    const box = element.getBoundingClientRect();
    const hit = element.getRootNode().elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    if (hit !== element && !element.contains(hit)) throw Error('Target is covered; inspect the page before clicking');
    element.click();
    return {clicked:true, method:'dom', trusted:false};
  }
  if (action === 'fill') {
    if (typeof args.text !== 'string') throw Error('text is required');
    const view = element.ownerDocument.defaultView;
    if (element instanceof view.HTMLInputElement || element instanceof view.HTMLTextAreaElement) {
      if (['file', 'checkbox', 'radio'].includes(element.type)) throw Error('Unsupported fill target');
      const proto = element instanceof view.HTMLTextAreaElement ? view.HTMLTextAreaElement.prototype : view.HTMLInputElement.prototype;
      Object.getOwnPropertyDescriptor(proto, 'value').set.call(element, args.text);
    } else if (element.isContentEditable) element.textContent = args.text;
    else throw Error('Target is not editable');
    element.dispatchEvent(new view.Event('input', {bubbles:true}));
    element.dispatchEvent(new view.Event('change', {bubbles:true}));
    return { filled: true };
  }
  const rect = element.getBoundingClientRect();
  const hit = element.getRootNode().elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
  if (hit !== element && !element.contains(hit)) throw Error('Target is covered; inspect the page before clicking');
  // scrollIntoView 可能同时滚动父页面；坐标必须在滚动后重新读取 frame 的偏移。
  let offsetX = 0, offsetY = 0;
  for (let view = element.ownerDocument.defaultView; view !== window; view = view.parent) {
    const frame = view.frameElement;
    if (!frame) throw Error('Frame coordinates unavailable');
    const bounds = frame.getBoundingClientRect();
    const frameHit = frame.getRootNode().elementFromPoint(bounds.left + frame.clientLeft + rect.left + rect.width / 2 + offsetX,
      bounds.top + frame.clientTop + rect.top + rect.height / 2 + offsetY);
    if (frameHit !== frame) throw Error('Frame target is covered; inspect the page before clicking');
    offsetX += bounds.left + frame.clientLeft;
    offsetY += bounds.top + frame.clientTop;
  }
  return { x: offsetX + rect.left + rect.width / 2, y: offsetY + rect.top + rect.height / 2 };
}
