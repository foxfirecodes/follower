(() => {
  'use strict';
  const source = JSON.parse(document.getElementById('csv-data').textContent);
  const title = JSON.parse(document.getElementById('csv-title').textContent);

  // RFC 4180 parsing: quoted fields may hold commas, quotes, and newlines.
  function parse(text) {
    const records = [];
    let record = [];
    let field = '';
    let quoted = false;
    for (let i = 0; i < text.length; i++) {
      const c = text[i];
      if (quoted) {
        if (c === '"' && text[i + 1] === '"') { field += '"'; i++; }
        else if (c === '"') quoted = false;
        else field += c;
      } else if (c === '"') quoted = true;
      else if (c === ',') { record.push(field); field = ''; }
      else if (c === '\n' || c === '\r') {
        if (c === '\r' && text[i + 1] === '\n') i++;
        record.push(field); records.push(record); record = []; field = '';
      } else field += c;
    }
    if (field || record.length) { record.push(field); records.push(record); }
    return records;
  }

  const [header, ...body] = parse(source);
  const rows = body.filter(record => record.length > 1).map(record => Object.fromEntries(header.map((column, index) => [column, record[index] || ''])));
  const has = column => header.includes(column);
  const itemColumn = header.find(column => column.startsWith('item.'));
  const argColumns = header.filter(column => column.startsWith('arg.'));
  const place = (path, line) => path ? `${path}:${line}` : '';
  rows.forEach(row => {
    row.callsite = place(row.callsite_path, row.callsite_line);
    row.call = place(row.call_path, row.call_line);
  });
  // A row needs review when the answer it gives is not complete.
  const review = row => row.status !== 'called' || row.arguments_resolved === 'false' || row.item_complete === 'false' || row.row_kind === 'no_call';

  const app = document.getElementById('app');
  app.innerHTML = `<main class="shell">
    <header class="top"><div><p class="eyebrow">Follower / Callsite table</p><h1 id="title"></h1><p class="sub" id="subtitle"></p></div></header>
    <div class="metrics" id="metrics"></div>
    <div class="notice" id="triage"></div>
    <div class="toolbar">
      <input class="search" id="search" type="search" placeholder="Search shown columns" aria-label="Search rows">
      <label class="field">Group by <select class="filter" id="group" aria-label="Group rows"></select></label>
      <span id="filters" class="filters"></span>
      <label class="check"><input type="checkbox" id="review"> Needs review</label>
      <label class="check"><input type="checkbox" id="excluded"> Excluded calls</label>
      <button class="button" id="columns-toggle" type="button">Columns</button>
      <span class="count" id="count"></span>
    </div>
    <div class="columns hidden" id="columns"></div>
    <div id="results"></div>
    <p class="footer">Generated from the CSV alone. A call no explored path ran is still a call in the source; <code>found</code> says how each was found, and <code>item_complete</code> whether every item it applies to is known.</p>
  </main>`;
  const $ = selector => document.querySelector(selector);
  const node = (tag, className, text) => {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = text;
    return element;
  };
  $('#title').textContent = title;
  $('#subtitle').textContent = `${rows.length} rows from the query CSV`;

  const distinct = column => new Set(rows.map(row => row[column]).filter(Boolean));
  const calls = rows.filter(row => row.row_kind === 'call');
  const metrics = [
    ['Callsites', distinct('callsite').size],
    ...(itemColumn ? [['Items', distinct(itemColumn).size]] : []),
    ['Calls', new Set(calls.map(row => row.call)).size],
    ['Unresolved calls', new Set(calls.filter(row => row.arguments_resolved === 'false').map(row => row.call)).size],
    ['Callsites to review', new Set(rows.filter(review).map(row => row.callsite)).size],
  ];
  metrics.forEach(([label, count]) => {
    const card = node('div', 'metric');
    card.append(node('strong', '', String(count)), node('span', '', label));
    $('#metrics').append(card);
  });
  const statuses = {};
  new Map(rows.map(row => [row.callsite, row.status])).forEach(status => { statuses[status] = (statuses[status] || 0) + 1; });
  $('#triage').textContent = `Callsites by result: ${Object.entries(statuses).sort((a, b) => b[1] - a[1]).map(([status, count]) => `${count} ${status.replaceAll('_', ' ')}`).join(', ')}. "Needs review" shows rows whose answer is not complete: a result not called or escaping, an unknown argument, or items that are not all known.`;

  // Columns: the callsite and call places stand for their path and line columns.
  const allColumns = ['row_kind', ...(itemColumn ? [itemColumn, 'item_complete'] : []), 'callsite', 'enclosing', 'reachability', 'unreached_reason', 'status', 'call', ...argColumns, 'arguments_resolved', 'found', 'context', 'via', 'instance', 'conditions', 'unfollowed', 'note', ...header.filter(column => column.startsWith('factory.'))].filter(column => has(column) || column === 'callsite' || column === 'call');
  const hiddenByDefault = new Set(['unreached_reason', 'context', 'via', 'instance', 'conditions', 'unfollowed', 'item_complete', 'arguments_resolved']);
  const visible = new Set(allColumns.filter(column => !hiddenByDefault.has(column) && !column.startsWith('factory.')));
  allColumns.forEach(column => {
    const label = node('label', 'check');
    const box = node('input');
    box.type = 'checkbox'; box.checked = visible.has(column);
    box.addEventListener('change', () => { box.checked ? visible.add(column) : visible.delete(column); render(); });
    label.append(box, document.createTextNode(` ${column}`));
    $('#columns').append(label);
  });
  $('#columns-toggle').addEventListener('click', () => $('#columns').classList.toggle('hidden'));

  const groupings = [['', 'Nothing'], ...(itemColumn ? [[itemColumn, itemColumn.slice(5)]] : []), ['callsite', 'Callsite'], ['call_path', 'Call file'], ...argColumns.map(column => [column, column.slice(4)]), ['status', 'Status'], ['enclosing', 'Enclosing function']];
  groupings.forEach(([value, label]) => { const option = node('option', '', label); option.value = value; $('#group').append(option); });
  $('#group').value = itemColumn || 'callsite';

  const filterColumns = ['row_kind', 'status', 'reachability', 'found', 'item_complete', 'arguments_resolved'].filter(has);
  const filters = {};
  filterColumns.forEach(column => {
    const select = node('select', 'filter');
    select.setAttribute('aria-label', `Filter ${column}`);
    const all = node('option', '', `${column}: any`); all.value = ''; select.append(all);
    [...distinct(column)].sort().forEach(value => { const option = node('option', '', `${column}: ${value}`); option.value = value; select.append(option); });
    select.addEventListener('change', () => { filters[column] = select.value; limit = 150; render(); });
    $('#filters').append(select);
  });

  let sort = { column: '', descending: false };
  let limit = 150;
  const chipClass = value => ({ called: 'exact', explored: 'exact', reachable: 'exact', true: 'exact', source: 'conditional', possible: 'conditional', inferred: 'conditional', escapes: 'unknown', unused: 'muted', not_called: 'muted', called_with_unknown_arguments: 'unknown', false: 'unknown', excluded_call: 'muted', no_call: 'muted', escape: 'unknown', unknown: 'unknown' })[value] || '';
  const chipColumns = new Set(['row_kind', 'status', 'reachability', 'found', 'item_complete', 'arguments_resolved']);

  function table(list, hide) {
    const columns = allColumns.filter(column => visible.has(column) && column !== hide);
    const element = node('table', 'grid');
    const head = node('tr');
    columns.forEach(column => {
      const th = node('th', sort.column === column ? 'sorted' : '', column + (sort.column === column ? (sort.descending ? ' ↓' : ' ↑') : ''));
      th.addEventListener('click', () => { sort = { column, descending: sort.column === column && !sort.descending }; render(); });
      head.append(th);
    });
    element.append(head);
    list.forEach(row => {
      const tr = node('tr', row.row_kind === 'call' ? '' : 'secondary');
      columns.forEach(column => {
        const value = row[column] || '';
        const td = node('td', column === 'note' || column === 'via' ? 'wide' : '');
        if (chipColumns.has(column) && value) td.append(node('span', `chip ${chipClass(value)}`, value.replaceAll('_', ' ')));
        else td.append(node('span', column === 'callsite' || column === 'call' || column === 'instance' ? 'mono' : '', value));
        tr.append(td);
      });
      element.append(tr);
    });
    return element;
  }

  function compare(a, b) {
    if (!sort.column) return 0;
    const left = a[sort.column] || ''; const right = b[sort.column] || '';
    const numeric = Number(left) - Number(right);
    const result = Number.isNaN(numeric) ? left.localeCompare(right) : numeric;
    return sort.descending ? -result : result;
  }

  function render() {
    const search = $('#search').value.trim().toLowerCase();
    const group = $('#group').value;
    const list = rows.filter(row =>
      ($('#excluded').checked || row.row_kind !== 'excluded_call')
      && (!$('#review').checked || review(row))
      && filterColumns.every(column => !filters[column] || row[column] === filters[column])
      // Only shown columns are searched, so a match is something on screen.
      && (!search || allColumns.some(column => visible.has(column) && (row[column] || '').toLowerCase().includes(search))))
      .sort(compare);
    $('#count').textContent = `${list.length} of ${rows.length} rows`;
    const results = $('#results');
    results.replaceChildren();
    if (!list.length) { results.append(node('p', 'empty', 'No matching rows.')); return; }
    if (!group) {
      results.append(table(list.slice(0, limit * 4)));
      if (list.length > limit * 4) results.append(more(list.length - limit * 4));
      return;
    }
    const groups = new Map();
    list.forEach(row => { const key = row[group] || '(none)'; if (!groups.has(key)) groups.set(key, []); groups.get(key).push(row); });
    // Known keys first; unknown items (`?reason`) and rows without a key last.
    const rank = key => key === '(none)' ? 2 : key.startsWith('?') ? 1 : 0;
    const keys = [...groups.keys()].sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
    keys.slice(0, limit).forEach(key => {
      const members = groups.get(key);
      const details = node('details', 'group');
      const summary = node('summary');
      summary.append(node('span', 'group-key mono', key.startsWith('?') ? `unknown (${key.slice(1).replaceAll('_', ' ')})` : key));
      const callsites = new Set(members.map(row => row.callsite)).size;
      const facts = [`${members.length} row${members.length === 1 ? '' : 's'}`, `${callsites} callsite${callsites === 1 ? '' : 's'}`];
      argColumns.filter(column => column !== group).forEach(column => {
        const values = [...new Set(members.filter(row => row.row_kind === 'call').map(row => row[column]).filter(Boolean))];
        if (values.length) facts.push(`${column.slice(4)}: ${values.join(', ')}`);
      });
      summary.append(node('span', 'group-facts', facts.join(' · ')));
      if (members.some(review)) summary.append(node('span', 'chip unknown', 'review'));
      details.append(summary);
      details.addEventListener('toggle', () => {
        if (details.open && details.childElementCount === 1) details.append(table(members, group));
      });
      // A search that narrows to a few groups opens them.
      if (search && keys.length <= 3) details.open = true;
      results.append(details);
    });
    if (keys.length > limit) results.append(more(keys.length - limit));
  }

  function more(remaining) {
    const button = node('button', 'load', `Show more (${remaining} more)`);
    button.type = 'button';
    button.addEventListener('click', () => { limit += 150; render(); });
    return button;
  }

  // `#q=text&group=column` sets the search and grouping, so a lookup can be linked.
  const hash = new URLSearchParams(location.hash.slice(1));
  if (hash.has('q')) $('#search').value = hash.get('q');
  if (hash.has('group') && groupings.some(([value]) => value === hash.get('group'))) $('#group').value = hash.get('group');
  ['#search', '#group', '#review', '#excluded'].forEach(selector => $(selector).addEventListener(selector === '#search' ? 'input' : 'change', () => { limit = 150; render(); }));
  render();
})();
