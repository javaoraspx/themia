// The To Do list's visual band, pinned from the stylesheet.
//
// [S-66]/[S-67] place the add row ABOVE the finished group, and they do it with
// flex `order` rather than by moving nodes: the `<For>` above must keep its
// identity, because the tick's glide ([S-65]) animates the elements it owns and
// the add field's half-typed stub lives in one of them. A well-meaning refactor
// that "just moves the add row down in the JSX" would compile, look almost
// right, and quietly break both. jsdom cannot cascade the stylesheet, so — like
// the other design-system contracts in this repo — the rule is read from the
// CSS source.
import { describe, it, expect } from 'vitest';
import { readSource, parseRules } from '../../../tests/cssRules.ts';

const rules = parseRules(readSource('widgets/office/todo/Todo.css'));

/** The `order` a selector declares, or null when no rule for it declares one.
 *  Scans past the selector's other rules (the settle tint/`.leaving` styles),
 *  so moving the declarations around cannot make this silently pass. */
function orderOf(selector: string): number | null {
  for (const r of rules) {
    if (r.selector !== selector) continue;
    const m = r.body.match(/order:\s*(-?\d+)/);
    if (m) return Number(m[1]);
  }
  return null;
}

describe('todo list visual band [S-66] [S-67]', () => {
  it('stacks active rows, add row, divider, finished rows — in that order', () => {
    expect(orderOf('.todo-item')).toBe(0);
    expect(orderOf('.todo-add-row')).toBe(1);
    expect(orderOf('.todo-divider')).toBe(2);
    expect(orderOf('.todo-item.in-done')).toBe(3);
  });

  // `.done` is the STATUS (and drives the strikethrough). A row in its beat has
  // already flipped status but must stay in the half it was ticked in ([S-65]),
  // so the band has to come from `rowBand` — a `.todo-item.done { order }` rule
  // would drag the row into the group on the first frame of its own beat.
  it('takes the band from the component, never from `done`', () => {
    expect(orderOf('.todo-item.done')).toBeNull();
  });

  it('only works because the list is a column flex container', () => {
    const list = rules.find(r => r.selector === '.todo-list')?.body ?? '';
    expect(list).toMatch(/display:\s*flex/);
    expect(list).toMatch(/flex-direction:\s*column/);
  });

  // A row gliding down to the finished group passes THROUGH the add row
  // ([S-65] runs 380ms after the tick, 260ms of movement). Without an opaque
  // fill and its own stacking level, two titles blend for that whole move.
  it('keeps the add row opaque and above the rows that travel through it', () => {
    const body = rules.find(r => r.selector === '.todo-add-row')?.body ?? '';
    expect(body, 'add row must be filled with the body surface').toMatch(/background:\s*var\(--card\)/);
    expect(body, 'add row must not be painted under a gliding row').toMatch(/z-index:\s*[1-9]/);
    expect(body, 'z-index only stacks a positioned box').toMatch(/position:\s*relative/);
  });

  it('draws the divider as the group boundary, not as a section header', () => {
    const body = rules.find(r => r.selector === '.todo-divider')?.body ?? '';
    expect(body).toMatch(/color:\s*var\(--subtle-foreground\)/);
    // The hairlines live on the pseudo elements, so the count sits between them.
    expect(rules.some(r => r.selector.includes('.todo-divider::before'))).toBe(true);
    expect(rules.some(r => r.selector.includes('.todo-divider::after'))).toBe(true);
  });
});
