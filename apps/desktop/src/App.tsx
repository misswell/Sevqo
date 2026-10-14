import { useEffect, useState, type ReactElement } from 'react';

import { Main } from './components/Main';
import { Spotlight } from './components/Spotlight';
import { applyScopeToDocument, detectScope, type WindowScope } from './scope';

export function App(): ReactElement {
  const [scope, setScope] = useState<WindowScope>(() => {
    const s = detectScope();
    applyScopeToDocument(s);
    return s;
  });

  // The label is fixed per window, but re-check after mount in case the bundle
  // booted before Tauri internals were injected.
  useEffect(() => {
    const next = detectScope();
    if (next !== scope) {
      setScope(next);
      applyScopeToDocument(next);
    }
  }, [scope]);

  return scope === 'spotlight' ? <Spotlight /> : <Main />;
}
