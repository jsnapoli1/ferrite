// Icons only the mail integrations use (travel, filters), drawn like ui.js icons; any other
// name falls back to the shared set.

import { icon } from '../ui.js';

const SVG_NS = 'http://www.w3.org/2000/svg';

const PATHS = {
  plane: 'M10.5 21l1.5-1.5V15l7 3v-2l-7-5V5.5a1.5 1.5 0 0 0-3 0V11l-7 5v2l7-3v4.5L10.5 21z',
  train: 'M6 4h12v11a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2zM6 10h12M9 14h.01M15 14h.01M8 21l2-4M16 21l-2-4',
  bus: 'M5 5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v12H5zM5 11h14M8 21v-4M16 21v-4M8 14h.01M16 14h.01',
  bed: 'M3 19V6M3 15h18v4M21 15v-3a3 3 0 0 0-3-3h-7v6M7 12a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z',
  ticket: 'M3 8a2 2 0 0 0 2-2h14a2 2 0 0 0 2 2v2a2 2 0 0 0 0 4v2a2 2 0 0 0-2 2H5a2 2 0 0 0-2-2v-2a2 2 0 0 0 0-4zM14 6v12',
  car: 'M5 16V11l2-5h10l2 5v5M3 16h18v3H3zM7.5 13.5h.01M16.5 13.5h.01M5 11h14',
  food: 'M7 3v8M5 3v5a2 2 0 0 0 4 0V3M7 11v10M17 21V3c-2 1-3 4-3 7h3',
  filter: 'M3 5h18l-7 8v6l-4 2v-8z',
  unsubscribe: 'M3 5h18v14H3zM3 6l9 7 9-7M16 16l4 4M20 16l-4 4',
  translate: 'M4 5h8M8 3v2M10 5c0 4-3 7-6 8M6 9c1 2 3 3.5 5 4M13 21l4-9 4 9M14.5 18h5',
  'chevron-up': 'M6 15l6-6 6 6',
  sparkle: 'M12 3l1.8 4.7L18.5 9.5l-4.7 1.8L12 16l-1.8-4.7L5.5 9.5l4.7-1.8z',
};

/** An icon by name: the integration icons above, else ui.js's. */
export function mi(name, cls = '') {
  const d = PATHS[name];
  if (!d) return icon(name, cls);
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', `icon ${cls}`.trim());
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(SVG_NS, 'path');
  path.setAttribute('d', d);
  svg.appendChild(path);
  return svg;
}
