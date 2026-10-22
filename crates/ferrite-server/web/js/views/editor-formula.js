// Spreadsheet formulas, evaluated client-side: numbers, + - * / and unary minus, parentheses,
// cell references (A1, $A$1), ranges (A1:B9) inside SUM / AVERAGE / MIN / MAX / COUNT, and
// string literals. Errors show as #ERR (bad syntax, text in arithmetic, division by zero),
// #REF (invalid reference) and #CYCLE (circular reference).

export const MAX_ROWS = 100000;
export const MAX_COLS = 1000;

export class SheetError extends Error {
  constructor(code) { super(code); this.code = code; }
}

/** 0-based column index → letters (0 → A, 26 → AA). */
export function colName(c) {
  let s = '';
  let n = c + 1;
  while (n > 0) {
    const m = (n - 1) % 26;
    s = String.fromCharCode(65 + m) + s;
    n = Math.floor((n - 1) / 26);
  }
  return s;
}

/** "B3" → {r: 2, c: 1}; null when malformed. */
export function parseRef(ref) {
  const m = /^\$?([A-Za-z]{1,3})\$?(\d{1,6})$/.exec(ref);
  if (!m) return null;
  let c = 0;
  for (const ch of m[1].toUpperCase()) c = c * 26 + (ch.charCodeAt(0) - 64);
  return { r: Number(m[2]) - 1, c: c - 1 };
}

function tokenize(src) {
  const out = [];
  let i = 0;
  while (i < src.length) {
    const ch = src[i];
    if (/\s/.test(ch)) { i++; continue; }
    let m;
    const rest = src.slice(i);
    if ((m = /^(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?/.exec(rest))) { out.push({ t: 'num', v: Number(m[0]) }); i += m[0].length; continue; }
    if ((m = /^\$?[A-Za-z]{1,3}\$?\d+(:\$?[A-Za-z]{1,3}\$?\d+)?/.exec(rest)) && !/^[A-Za-z]+\(/.test(rest)) {
      out.push({ t: m[1] ? 'range' : 'ref', v: m[0] }); i += m[0].length; continue;
    }
    if ((m = /^[A-Za-z_][A-Za-z0-9_.]*/.exec(rest))) { out.push({ t: 'name', v: m[0].toUpperCase() }); i += m[0].length; continue; }
    if (ch === '"') {
      const end = src.indexOf('"', i + 1);
      if (end < 0) throw new SheetError('#ERR');
      out.push({ t: 'str', v: src.slice(i + 1, end) });
      i = end + 1;
      continue;
    }
    if ('+-*/(),:'.includes(ch)) { out.push({ t: ch }); i++; continue; }
    throw new SheetError('#ERR');
  }
  return out;
}

const FUNCS = {
  SUM: (xs) => xs.reduce((a, b) => a + b, 0),
  AVERAGE: (xs) => { if (!xs.length) throw new SheetError('#ERR'); return xs.reduce((a, b) => a + b, 0) / xs.length; },
  AVG: (xs) => FUNCS.AVERAGE(xs),
  MIN: (xs) => (xs.length ? Math.min(...xs) : 0),
  MAX: (xs) => (xs.length ? Math.max(...xs) : 0),
  COUNT: (xs) => xs.length,
};

/**
 * Evaluates a whole sheet. `raw(r, c)` returns a cell's stored string. Returns a function
 * value(r, c) → number | string | SheetError (memoized, cycle-safe).
 */
export function evaluator(raw) {
  const memo = new Map();
  const visiting = new Set();

  function value(r, c) {
    if (r < 0 || c < 0 || r >= MAX_ROWS || c >= MAX_COLS) return new SheetError('#REF');
    const key = `${r},${c}`;
    if (memo.has(key)) return memo.get(key);
    if (visiting.has(key)) return new SheetError('#CYCLE');
    const src = raw(r, c);
    let v;
    if (src === undefined || src === null || src === '') v = '';
    else if (typeof src !== 'string') v = src;
    else if (src[0] === '=' && src.length > 1) {
      visiting.add(key);
      try {
        v = evalFormula(src.slice(1));
      } catch (e) {
        v = e instanceof SheetError ? e : new SheetError('#ERR');
      }
      visiting.delete(key);
    } else {
      const t = src.trim();
      v = t !== '' && /^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/.test(t) ? Number(t) : src;
    }
    memo.set(key, v);
    return v;
  }

  function evalFormula(src) {
    const toks = tokenize(src);
    let pos = 0;
    const peek = () => toks[pos];
    const eat = (t) => {
      if (!toks[pos] || toks[pos].t !== t) throw new SheetError('#ERR');
      return toks[pos++];
    };
    const num = (v) => {
      if (v instanceof SheetError) throw v;
      if (v === '') return 0;
      if (typeof v === 'number') return v;
      throw new SheetError('#ERR');
    };
    function refValue(ref) {
      const p = parseRef(ref);
      if (!p) throw new SheetError('#REF');
      return value(p.r, p.c);
    }
    function rangeValues(range) {
      const [a, b] = range.split(':').map(parseRef);
      if (!a || !b) throw new SheetError('#REF');
      const out = [];
      const [r0, r1] = [Math.min(a.r, b.r), Math.max(a.r, b.r)];
      const [c0, c1] = [Math.min(a.c, b.c), Math.max(a.c, b.c)];
      if ((r1 - r0 + 1) * (c1 - c0 + 1) > 200000) throw new SheetError('#REF');
      for (let r = r0; r <= r1; r++) {
        for (let c = c0; c <= c1; c++) {
          const v = value(r, c);
          if (v instanceof SheetError) throw v;
          if (typeof v === 'number') out.push(v);
        }
      }
      return out;
    }
    function expr() {
      let v = term();
      while (peek() && (peek().t === '+' || peek().t === '-')) {
        const op = toks[pos++].t;
        const r = term();
        v = op === '+' ? num(v) + num(r) : num(v) - num(r);
      }
      return v;
    }
    function term() {
      let v = unary();
      while (peek() && (peek().t === '*' || peek().t === '/')) {
        const op = toks[pos++].t;
        const r = unary();
        if (op === '*') v = num(v) * num(r);
        else {
          const d = num(r);
          if (d === 0) throw new SheetError('#ERR');
          v = num(v) / d;
        }
      }
      return v;
    }
    function unary() {
      if (peek() && peek().t === '-') { pos++; return -num(unary()); }
      if (peek() && peek().t === '+') { pos++; return num(unary()); }
      return atom();
    }
    function atom() {
      const t = peek();
      if (!t) throw new SheetError('#ERR');
      pos++;
      if (t.t === 'num') return t.v;
      if (t.t === 'str') return t.v;
      if (t.t === 'ref') { const v = refValue(t.v); if (v instanceof SheetError) throw v; return v; }
      if (t.t === '(') { const v = expr(); eat(')'); return v; }
      if (t.t === 'name') {
        const fn = FUNCS[t.v];
        if (!fn) throw new SheetError('#ERR');
        eat('(');
        const xs = [];
        if (peek() && peek().t !== ')') {
          for (;;) {
            const a = peek();
            if (a && a.t === 'range') { pos++; xs.push(...rangeValues(a.v)); } else {
              const v = expr();
              if (v instanceof SheetError) throw v;
              if (typeof v === 'number') xs.push(v);
              else if (v !== '') throw new SheetError('#ERR');
            }
            if (peek() && peek().t === ',') { pos++; continue; }
            break;
          }
        }
        eat(')');
        return fn(xs);
      }
      if (t.t === 'range') throw new SheetError('#ERR');
      throw new SheetError('#ERR');
    }
    const v = expr();
    if (pos !== toks.length) throw new SheetError('#ERR');
    if (typeof v === 'number' && !Number.isFinite(v)) throw new SheetError('#ERR');
    return v;
  }

  return value;
}

/** Display text of an evaluated value. */
export function display(v) {
  if (v instanceof SheetError) return v.code;
  if (typeof v === 'number') {
    if (Number.isInteger(v)) return String(v);
    return String(Number(v.toPrecision(12)));
  }
  return String(v ?? '');
}
