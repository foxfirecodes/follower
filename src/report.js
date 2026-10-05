(() => {
  'use strict';
  const report = JSON.parse(document.getElementById('report-data').textContent);
  const app = document.getElementById('app');
  app.innerHTML = `<main class="shell">
    <header class="top"><div><p class="eyebrow">Follower / Query report</p><h1 id="title"></h1><p class="sub" id="subtitle"></p></div><div class="snapshot mono" id="snapshot"></div></header>
    <div class="metrics" id="metrics"></div>
    <nav class="tabs" aria-label="Report sections"><button class="tab active" data-tab="callsites">Callsites</button><button class="tab" data-tab="creations">Creations</button><button class="tab" data-tab="boundaries">Boundaries</button><button class="tab" data-tab="unreached">Unreached</button><button class="tab" data-tab="coverage">Coverage</button><button class="tab" data-tab="evidence">Evidence</button></nav>
    <section id="callsites" class="panel active"><div class="notice" id="callsite-notice"></div><div class="evidence-card"><div class="toolbar"><input class="search" id="callsite-search" type="search" placeholder="Search file, value, or context" aria-label="Search callsites"><select class="filter" id="callsite-filter" aria-label="Filter result status"><option value="all">All results</option><option value="called">Called</option><option value="called_with_unknown_arguments">Called with unknown arguments</option><option value="escapes">Escapes</option><option value="not_called">Not called</option><option value="unused">Unused</option></select><span class="count" id="callsite-count"></span></div><div id="callsite-list" class="stack"></div></div></section>
    <section id="creations" class="panel"><div class="toolbar"><input class="search" id="creation-search" type="search" placeholder="Search creation, choice, value, or file" aria-label="Search creations"><select class="filter" id="creation-filter" aria-label="Filter creation status"><option value="all">All conclusions</option><option value="candidate_invocation">Invoked</option><option value="unresolved">Unresolved</option><option value="absent_within_model">No invocation</option></select><span class="count" id="creation-count"></span></div><div class="split"><div class="list"><div class="list-title">Creation contexts</div><div id="creation-list"></div></div><div class="detail" id="creation-detail"></div></div></section>
    <section id="coverage" class="panel"><div class="notice" id="coverage-notice"></div><div class="coverage-grid"><div class="coverage-card"><header><h2>Coverage gaps</h2><span class="count" id="gap-count"></span></header><div class="toolbar"><input class="search" id="gap-search" type="search" placeholder="Search gaps" aria-label="Search gaps"><select class="filter" id="gap-filter" aria-label="Filter gap assessment"><option value="all">All assessments</option><option value="direct">Direct</option><option value="may_affect">May affect</option><option value="unknown_relevance">Unknown relevance</option><option value="unlinked">Unlinked</option></select></div><div id="gap-list"></div></div><div class="coverage-card"><header><h2>Callsite inventory</h2><span class="count" id="inventory-count"></span></header><div class="toolbar"><input class="search" id="inventory-search" type="search" placeholder="Search file or reason" aria-label="Search callsites"><select class="filter" id="inventory-filter" aria-label="Filter callsite status"><option value="all">All callsites</option><option value="analyzed">Analyzed</option><option value="filtered">Filtered</option><option value="unresolved">Unresolved</option><option value="skipped">Skipped</option></select></div><div id="inventory-list"></div></div></div></section>
    <section id="boundaries" class="panel"><div class="notice" id="boundary-notice"></div><div class="evidence-card"><div class="toolbar"><input class="search" id="boundary-search" type="search" placeholder="Search component, module, or file" aria-label="Search boundaries"><span class="count" id="boundary-count"></span></div><div id="boundary-list" class="stack"></div></div></section>
    <section id="unreached" class="panel"><div class="notice" id="unreached-notice"></div><div class="evidence-card"><div class="toolbar"><input class="search" id="unreached-search" type="search" placeholder="Search file, reason, or blocking site" aria-label="Search unreached callsites"><span class="count" id="unreached-count"></span></div><div id="unreached-list" class="stack"></div></div></section>
    <section id="evidence" class="panel"><div class="evidence-card"><div class="toolbar"><input class="search" id="evidence-search" type="search" placeholder="Search evidence ID, rule, summary" aria-label="Search evidence"><span class="count" id="evidence-count"></span></div><div id="evidence-list" class="evidence-list"></div></div></section>
    <p class="footer">Static analysis describes possible flow within configured roots and model limits. A candidate invocation does not prove runtime execution. This file is self-contained and stored locally.</p>
  </main>`;

  const $ = selector => document.querySelector(selector);
  const node = (tag, className, content) => {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (content !== undefined) element.textContent = String(content);
    return element;
  };
  const button = (label, handler, className='button') => {
    const element = node('button', className, label);
    element.type = 'button';
    element.addEventListener('click', handler);
    return element;
  };
  const badge = (label, type=label) => node('span', `chip ${type}`, label.replaceAll('_', ' '));
  const loc = location => location ? `${location.path}:${location.start_line}:${location.start_column}` : 'location unavailable';
  const pathLine = (location, prefix='') => location ? `${location.path.startsWith(prefix) ? location.path.slice(prefix.length) : location.path}:${location.start_line}` : 'location unavailable';
  const sharedDirectory = locations => {
    let prefix = locations[0]?.path.slice(0, locations[0].path.lastIndexOf('/') + 1) || '';
    while (prefix && locations.some(location => !location.path.startsWith(prefix))) {
      const previous = prefix.slice(0, -1);
      prefix = previous.slice(0, previous.lastIndexOf('/') + 1);
    }
    return prefix;
  };
  const status = creation => creation.conclusion === 'candidate_invocation' ? 'candidate invocation' : creation.conclusion === 'absent_within_model' ? 'no invocation' : 'unresolved';
  const valueText = value => {
    if (!value) return '—';
    switch (value.kind) {
      case 'null': return 'null';
      case 'undefined': return 'undefined';
      case 'string': return JSON.stringify(value.value);
      case 'number': case 'boolean': return String(value.value);
      case 'enum_member': return `${value.enum_name}.${value.member_name}`;
      case 'array': return `[${value.elements.map(valueText).join(', ')}]`;
      case 'alternatives': return value.values.map(valueText).join('  |  ');
      case 'unknown': return `unknown (${value.reason})`;
      default: return JSON.stringify(value);
    }
  };
  const certainty = value => {
    if (!value) return 'unknown';
    if (value.kind === 'unknown') return 'unknown';
    const children = value.kind === 'array' ? value.elements : value.kind === 'alternatives' ? value.values : [];
    if (children.some(child => certainty(child) === 'unknown')) return 'unknown';
    if (value.kind === 'alternatives' || children.some(child => certainty(child) === 'conditional')) return 'conditional';
    return 'exact';
  };
  const evidenceById = new Map(report.evidence.map(item => [`E${item.id}`, item]));
  const creationById = new Map(report.creations.map(item => [item.creation_id, item]));
  const tabs = [...document.querySelectorAll('.tab')];
  function showTab(name) {
    tabs.forEach(tab => tab.classList.toggle('active', tab.dataset.tab === name));
    document.querySelectorAll('.panel').forEach(panel => panel.classList.toggle('active', panel.id === name));
  }
  tabs.forEach(tab => tab.addEventListener('click', () => showTab(tab.dataset.tab)));

  $('#title').textContent = report.query_id;
  $('#subtitle').textContent = `${report.kind.replaceAll('_', ' ')} · ${report.scope.replaceAll('_', ' ')}`;
  $('#snapshot').textContent = `snapshot ${report.snapshot_id.slice(0, 16)} · schema ${report.schema_version}`;
  const unresolvedCount = report.creations.filter(creation => creation.unresolved_count > 0).length;
  const callsites = report.callsites || [];
  const metrics = [
    ['Callsites', callsites.length || report.callsite_inventory.callsites.length],
    ['Calls found', callsites.reduce((sum, callsite) => sum + callsite.capability.calls.length, 0)],
    ['Creations', report.creations.length],
    ['Unresolved rows', unresolvedCount],
    ['Boundaries', (report.component_boundaries || []).length],
    ['Unreached callsites', (report.unreached_callsites || []).length],
    ['Coverage gaps', report.gaps.length],
  ];
  metrics.forEach(([label, count]) => {
    const card = node('div', `metric ${label === 'Coverage gaps' && count ? 'alert' : ''}`);
    card.append(node('strong', '', count), node('span', '', label));
    $('#metrics').append(card);
  });

  let selectedCreation = report.creations[0]?.creation_id;
  let creationLimit = 120;
  const creationsBySite = [...report.creations].sort((a, b) =>
    loc(a.factory_location).localeCompare(loc(b.factory_location)) || a.choice.localeCompare(b.choice));
  const creationSearch = $('#creation-search');
  const creationFilter = $('#creation-filter');
  const creationHaystack = creation => [creation.creation_id, creation.choice, loc(creation.factory_location), creation.reverse_importer?.symbol, creation.reverse_importer?.location && loc(creation.reverse_importer.location), ...Object.values(creation.factory_arguments).map(valueText), ...creation.invocations.flatMap(invocation => Object.values(invocation.arguments).map(valueText))].join(' ').toLowerCase();
  function renderCreationList() {
    const search = creationSearch.value.trim().toLowerCase();
    const filtered = creationsBySite.filter(creation => (creationFilter.value === 'all' || creation.conclusion === creationFilter.value) && (!search || creationHaystack(creation).includes(search)));
    $('#creation-count').textContent = `${filtered.length} of ${report.creations.length}`;
    const list = $('#creation-list');
    list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching creations.')); return; }
    const fragment = document.createDocumentFragment();
    let group = '';
    filtered.slice(0, creationLimit).forEach(creation => {
      const site = loc(creation.factory_location);
      if (site !== group) { group = site; fragment.append(node('div', 'group-title mono', site)); }
      const row = button('', () => { selectedCreation = creation.creation_id; renderCreationList(); renderCreationDetail(); }, `row ${creation.creation_id === selectedCreation ? 'active' : ''}`);
      const main = node('div', 'row-main');
      main.append(node('span', '', creation.choice || 'default context'), badge(status(creation), creation.conclusion));
      if (creation.reachability !== 'reachable') main.append(reachabilityBadge(creation));
      row.append(main, node('div', 'row-small mono', creation.creation_id));
      if (creation.reverse_importer) row.append(node('div', 'row-small', `via ${creation.reverse_importer.symbol || 'module binding'} · ${loc(creation.reverse_importer.location)}`));
      fragment.append(row);
    });
    list.append(fragment);
    if (filtered.length > creationLimit) list.append(button(`Show more (${filtered.length - creationLimit} remaining)`, () => { creationLimit += 120; renderCreationList(); }, 'load'));
  }
  function valueRows(values, evidenceMap={}) {
    const wrap = node('div');
    Object.entries(values).forEach(([label, value]) => {
      const row = node('div', 'value-row');
      row.append(node('span', 'value-label', label), badge(certainty(value)), node('span', 'value-text mono', valueText(value)));
      const evidenceId = evidenceMap[label];
      if (evidenceId) row.append(button(evidenceId, () => showEvidence(evidenceId)));
      wrap.append(row);
    });
    if (!Object.keys(values).length) wrap.append(node('p', 'muted', 'No projected arguments.'));
    return wrap;
  }
  function relatedGaps(creation) {
    return report.gaps.filter(gap => gap.links.some(link => link.creation_id === creation.creation_id));
  }
  function reachabilityBadge(creation) {
    if (creation.reachability === 'possible') return badge('reachability possible', 'conditional');
    if (creation.reachability === 'declared') return badge('reachability declared', 'conditional');
    return badge('reachability unknown', 'unknown');
  }
  function callPath(invocation, reachability) {
    const section = node('div', 'call-path');
    section.append(node('h4', '', reachability === 'reachable' ? 'Path to this invocation' : reachability === 'possible' ? 'Assumed path to this invocation' : reachability === 'declared' ? 'Declared path to this invocation' : 'Local path to this invocation'));
    if (invocation.other_paths) section.append(node('p', 'aside', `${invocation.other_paths} other explored path${invocation.other_paths === 1 ? '' : 's'} reach this invocation with the same values; this is the first.`));
    const full = invocation.call_path || [];
    if (!full.length) {
      section.append(node('p', 'muted', 'No path was recorded. Rerun the query with the current engine.'));
      return section;
    }
    const prefix = sharedDirectory(full.map(step => step.location));
    if (prefix) section.append(node('p', 'call-root mono muted', `Relative to ${prefix}`));
    const compact = full.filter((step, index) => index === 0 || index === full.length - 1 || step.location.path !== full[index + 1].location.path || step.kind === 'factory' || step.kind === 'invocation' || step.kind === 'assumed_render' || step.kind === 'uncalled_callback' || step.kind === 'declared_render');
    const chain = node('ol', 'call-chain');
    let expanded = false;
    const toggle = button('', () => { expanded = !expanded; renderSteps(); });
    function renderSteps() {
      const steps = expanded ? full : compact;
      chain.replaceChildren();
      steps.forEach(step => {
        const item = node('li', 'call-step');
        item.append(node('span', 'call-kind', step.kind.replaceAll('_', ' ')), node('span', 'mono call-location', pathLine(step.location, prefix)));
        chain.append(item);
      });
      toggle.textContent = expanded ? 'Show file transitions' : `Show all ${full.length} steps`;
    }
    renderSteps();
    section.append(chain);
    if (compact.length < full.length) section.append(toggle);
    const reach = reachability === 'reachable' ? 'A possible path from a configured entry.' : reachability === 'possible' ? 'A path from a configured entry that holds only if each assumed render step renders what it was given; those components are not analyzed and each is listed as a gap.' : reachability === 'declared' ? 'A path from a render root or render call the project declares, not from a configured entry.' : 'Entry reachability is unknown; this path starts at a locally explored call.';
    section.append(node('p', 'aside', `${reach} The compact view shows cross-file handoffs plus creation and invocation; the full view includes same-file calls and renders. Factory creation and callback invocation may occur at different times.`));
    return section;
  }
  function renderCreationDetail() {
    const detail = $('#creation-detail');
    detail.replaceChildren();
    const creation = creationById.get(selectedCreation);
    if (!creation) { detail.append(node('p', 'empty', 'Choose a creation context to inspect its flow.')); return; }
    const head = node('div', 'detail-head');
    const title = node('div');
    title.append(node('p', 'eyebrow', 'Creation context'), node('h2', '', creation.choice || 'default context'), node('p', 'mono muted', loc(creation.factory_location)));
    head.append(title, badge(status(creation), creation.conclusion));
    if (creation.reachability !== 'reachable') head.append(reachabilityBadge(creation));
    detail.append(head);
    const definition = node('dl', 'kv');
    [['Creation ID', creation.creation_id], ['Reachability', creation.reachability], ['Capability path', creation.capability_path.join(' → ') || 'return value'], ['Registrations', creation.registrations.length], ['Invocations', creation.invocations.length], ['Unresolved', creation.unresolved_count]].forEach(([label, value]) => definition.append(node('dt', '', label), node('dd', 'mono', value)));
    detail.append(definition);
    if (creation.unresolved_count > creation.unresolved.length) detail.append(node('p', 'aside', 'Unresolved details were omitted by the query report option; coverage gaps and evidence remain available.'));
    if (creation.reverse_importer) {
      const seed = creation.reverse_importer;
      const origin = node('div', 'section');
      origin.append(node('h3', '', 'Reverse importer seed'));
      const description = seed.evaluation === 'direct_import_use' ? 'Direct imported use in a large file' : seed.evaluation === 'module_binding' ? 'Module binding' : 'Function body';
      origin.append(node('p', '', `${seed.symbol || 'module binding'} · ${description}`), node('p', 'mono muted', loc(seed.location)));
      origin.append(node('p', 'mini muted', `Matched imports: ${seed.matched_imports.join(', ') || 'unknown'}`));
      detail.append(origin);
    }
    const factory = node('div', 'section'); factory.append(node('h3', '', 'Factory arguments'), valueRows(creation.factory_arguments, creation.factory_argument_evidence)); detail.append(factory);
    const invocations = node('div', 'section'); invocations.append(node('h3', '', `Invocations (${creation.invocations.length})`));
    creation.invocations.forEach(invocation => {
      const card = node('div', 'invocation'); const top = node('div', 'invocation-head');
      top.append(node('span', 'mono', loc(invocation.location)), button(invocation.evidence_id, () => showEvidence(invocation.evidence_id)));
      card.append(top, valueRows(invocation.arguments, invocation.argument_evidence), callPath(invocation, creation.reachability)); invocations.append(card);
    });
    if (!creation.invocations.length) invocations.append(node('p', 'muted', 'No invocation found within the explored model.'));
    detail.append(invocations);
    const gaps = relatedGaps(creation);
    const caveats = node('div', 'section'); caveats.append(node('h3', '', `Linked coverage gaps (${gaps.length})`));
    gaps.forEach(gap => { const line = node('div', 'mini'); line.append(badge(gap.assessment), node('span', '', ` ${gap.summary}`)); caveats.append(line); });
    if (!gaps.length) caveats.append(node('p', 'muted', 'No gap was directly linked to this creation. Check Coverage for gaps with unknown relevance.'));
    detail.append(caveats);
  }
  creationSearch.addEventListener('input', () => { creationLimit = 120; renderCreationList(); });
  creationFilter.addEventListener('change', () => { creationLimit = 120; renderCreationList(); });

  $('#coverage-notice').textContent = report.coverage.complete ? 'Coverage is complete within the configured model and source scope.' : 'Coverage is incomplete. Direct links are proven by solver evidence; “may affect” is possible; unknown relevance and unlinked gaps need manual review.';
  let gapLimit = 100;
  function renderGaps() {
    const search = $('#gap-search').value.trim().toLowerCase();
    const filter = $('#gap-filter').value;
    const filtered = report.gaps.filter(gap => (filter === 'all' || gap.assessment === filter) && (!search || `${gap.kind} ${gap.summary} ${loc(gap.location)}`.toLowerCase().includes(search)));
    $('#gap-count').textContent = `${filtered.length} of ${report.gaps.length}`;
    const list = $('#gap-list'); list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching gaps.')); return; }
    const counts = new Map(); filtered.forEach(gap => counts.set(gap.kind, (counts.get(gap.kind) || 0) + 1));
    const grouped = [...filtered].sort((a,b) => a.kind.localeCompare(b.kind) || a.gap_id.localeCompare(b.gap_id));
    let group = '';
    grouped.slice(0, gapLimit).forEach(gap => {
      if (gap.kind !== group) { group = gap.kind; list.append(node('div', 'group-title', `${group.replaceAll('_', ' ')} · ${counts.get(group)}`)); }
      const card = node('details', 'gap'); const summary = node('summary'); summary.append(badge(gap.assessment), node('span', 'gap-kind', gap.summary)); card.append(summary);
      const body = node('div', 'gap-body'); body.append(node('div', 'mono', `${gap.gap_id} · ${loc(gap.location)}`));
      if (gap.choice) body.append(node('div', 'mini', `Context: ${gap.choice}`));
      gap.links.forEach(link => {
        const line = node('div', 'mini'); line.append(badge(link.assessment));
        const destination = link.creation_id || (link.callsite_index !== null ? `callsite #${link.callsite_index + 1}` : 'unknown target');
        line.append(node('span', '', ` ${link.target.replaceAll('_',' ')}: ${destination}${link.label ? ` / ${link.label}` : ''}`));
        if (link.creation_id) line.append(button('View creation', () => { selectedCreation = link.creation_id; renderCreationList(); renderCreationDetail(); showTab('creations'); }));
        if (link.evidence_path?.length) line.append(button('Evidence path', () => showEvidence(link.evidence_path[0])));
        body.append(line);
      });
      if (!gap.links.length) body.append(node('p', 'mini', 'No target link could be established.'));
      card.append(body); list.append(card);
    });
    if (grouped.length > gapLimit) list.append(button(`Show more (${grouped.length - gapLimit} remaining)`, () => { gapLimit += 100; renderGaps(); }, 'load'));
  }
  const boundaries = report.component_boundaries || [];
  $('#boundary-notice').textContent = boundaries.length
    ? 'Possible creations depend on these components, most consequential first. Start with boundaries reached exactly. A suggested contract is an assumption about the component: check that it renders what it receives before adding one to the project definition.'
    : 'No component boundary affects the reported creations.';
  function renderBoundaries() {
    const search = $('#boundary-search').value.trim().toLowerCase();
    const filtered = boundaries.filter(boundary => !search || [boundary.component, boundary.module, boundary.export, boundary.kind, ...boundary.sites.map(loc)].join(' ').toLowerCase().includes(search));
    $('#boundary-count').textContent = `${filtered.length} of ${boundaries.length}`;
    const list = $('#boundary-list'); list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching boundaries.')); return; }
    filtered.forEach(boundary => {
      const card = node('details', 'gap'); const summary = node('summary');
      summary.append(badge(boundary.kind, boundary.entered_from_reachable ? 'conditional' : 'unknown'), node('span', 'gap-kind', `${boundary.component}${boundary.module ? ` from ${boundary.module}#${boundary.export}` : ''}`), node('span', 'count', `${boundary.affected_creations} creations · ${boundary.sole_blocker_creations} only through it`));
      card.append(summary);
      const body = node('div', 'gap-body');
      body.append(node('div', 'mono', `${boundary.boundary_id}${boundary.entered_from_reachable ? ' · reached exactly' : ''} · ${boundary.site_count} site${boundary.site_count === 1 ? '' : 's'}`));
      body.append(node('p', 'mini', boundary.reason));
      boundary.sites.forEach(site => body.append(node('div', 'mini mono', loc(site))));
      if (boundary.suggested_contract) { body.append(node('h3', 'mini', 'Suggested contract')); body.append(node('pre', 'mono', boundary.suggested_contract)); }
      card.append(body); list.append(card);
    });
  }
  $('#boundary-search').addEventListener('input', renderBoundaries);
  $('#callsite-notice').textContent = 'Each matching factory callsite with the values it was called with and the calls made with its result. Calls are found in the source, so a call no explored path executed is still listed; reachability says whether a path from a configured root reaches the callsite.';
  const arrayCount = value => value.kind === 'array' ? 1 : value.kind === 'alternatives' ? value.values.reduce((sum, item) => sum + arrayCount(item), 0) : 0;
  const arrayList = values => values.flatMap(value => value.kind === 'array' ? [value] : value.kind === 'alternatives' ? arrayList(value.values) : []);
  const callsiteCertainty = status => status === 'called' ? 'exact' : status === 'unused' || status === 'not_called' ? 'conditional' : 'unknown';
  function renderCallsites() {
    const search = $('#callsite-search').value.trim().toLowerCase();
    const status = $('#callsite-filter').value;
    const filtered = callsites.filter(item => (status === 'all' || item.capability.status === status) && (!search || [loc(item.location), item.enclosing, item.reachability, ...Object.values(item.factory_arguments).flat().map(valueText), ...Object.values(item.possible_elements || {}).flat().map(valueText), ...Object.values(item.values_from_callers || {}).flat().map(caller => valueText(caller.value)), ...item.capability.calls.flatMap(call => [loc(call.location), ...call.context, ...call.via, ...Object.values(call.arguments).flat().map(valueText)]), ...item.capability.escapes.map(escape => escape.detail)].join(' ').toLowerCase().includes(search)));
    $('#callsite-count').textContent = `${filtered.length} of ${callsites.length}`;
    const list = $('#callsite-list'); list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching callsites.')); return; }
    filtered.forEach(item => {
      const card = node('details', 'gap'); const summary = node('summary');
      summary.append(badge(item.capability.status.replaceAll('_', ' '), callsiteCertainty(item.capability.status)), node('span', 'gap-kind', loc(item.location)));
      card.append(summary);
      const body = node('div', 'gap-body');
      if (item.enclosing) body.append(node('div', 'mini', `In ${item.enclosing}`));
      body.append(node('div', 'mini', `Reachability: ${item.reachability}${item.unreached_reason ? ` (${item.unreached_reason.replaceAll('_', ' ')})` : ''} · ${item.contexts} explored contexts`));
      Object.entries(item.factory_arguments).forEach(([label, values]) => {
        const arrays = values.reduce((sum, value) => sum + arrayCount(value), 0);
        const elements = (item.possible_elements || {})[label];
        if (elements && arrays > 4) {
          // Many possible arrays read better as the elements they are drawn from.
          body.append(node('div', 'mini mono', `${label} = one of ${arrays} arrays of: ${elements.map(valueText).join(', ')}`));
          const all = node('details', 'mini'); all.append(node('summary', '', `Show all ${arrays} arrays`));
          arrayList(values).forEach(array => all.append(node('div', 'mini mono', valueText(array))));
          body.append(all);
        } else {
          body.append(node('div', 'mini mono', `${label} = ${values.map(valueText).join(' | ')}`));
          if (elements && arrays <= 1) body.append(node('div', 'mini mono', `${label} may contain ${elements.map(valueText).join(', ')}`));
        }
      });
      Object.entries(item.values_from_callers || {}).forEach(([label, callers]) => callers.forEach(caller => body.append(node('div', 'mini mono', `${label} = ${valueText(caller.value)} from ${loc(caller.caller)}`))));
      item.capability.calls.forEach(call => {
        const args = Object.entries(call.arguments).map(([label, values]) => `${label} = ${values.map(valueText).join(' | ')}`).join(', ');
        body.append(node('h3', 'mini', `Call at ${loc(call.location)}${call.explored ? '' : ' · not executed by an explored path'}`));
        body.append(node('div', 'mini mono', args || 'no projected arguments'));
        Object.entries(call.elements || {}).forEach(([label, values]) => body.append(node('div', 'mini mono', `for ${label}: ${values.map(valueText).join(', ') || 'none'}${call.elements_complete ? '' : ' (incomplete)'}`)));
        if (call.instance) body.append(node('div', 'mini', `Instance at ${loc(call.instance)}`));
        if (call.context.length) body.append(node('div', 'mini', `In ${call.context.join(', in ')}`));
        if (call.via.length) body.append(node('div', 'mini', `Via ${call.via.join(' → ')}`));
      });
      (item.capability.excluded_calls || []).forEach(call => body.append(node('div', 'mini', `Excluded call at ${loc(call.location)}: its conditions (${call.guards.map(set => set.map(valueText).join(' | ')).join('; ')}) match nothing this callsite requests`)));
      item.capability.escapes.forEach(escape => body.append(node('div', 'mini', `Escapes at ${loc(escape.location)}: ${escape.detail}`)));
      card.append(body); list.append(card);
    });
  }
  $('#callsite-search').addEventListener('input', renderCallsites);
  $('#callsite-filter').addEventListener('change', renderCallsites);
  const unreached = report.unreached_callsites || [];
  $('#unreached-notice').textContent = unreached.length
    ? 'Factory callsites no exact or possible path from a configured root reached. Each names the nearest code explored exactly and why exploration did not follow the use inside it.'
    : 'Every matching factory callsite was reached from a configured root.';
  function renderUnreached() {
    const search = $('#unreached-search').value.trim().toLowerCase();
    const filtered = unreached.filter(item => !search || [item.reason, item.detail, item.enclosing, loc(item.location), item.blocking_site && loc(item.blocking_site)].join(' ').toLowerCase().includes(search));
    $('#unreached-count').textContent = `${filtered.length} of ${unreached.length}`;
    const list = $('#unreached-list'); list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching callsites.')); return; }
    filtered.forEach(item => {
      const card = node('details', 'gap'); const summary = node('summary');
      summary.append(badge(item.reason, 'unknown'), node('span', 'gap-kind', loc(item.location)));
      card.append(summary);
      const body = node('div', 'gap-body');
      if (item.enclosing) body.append(node('div', 'mini', `In ${item.enclosing}`));
      body.append(node('p', 'mini', item.detail));
      if (item.explored_ancestor) body.append(node('div', 'mini', `Explored: ${item.explored_ancestor}`));
      if (item.blocking_site) body.append(node('div', 'mini mono', `Blocking use: ${loc(item.blocking_site)}`));
      item.chain.forEach(step => body.append(node('div', 'mini mono', `← ${step}`)));
      if (item.suggested_contract) { body.append(node('h3', 'mini', 'Suggested contract')); body.append(node('pre', 'mono', item.suggested_contract)); }
      card.append(body); list.append(card);
    });
  }
  $('#unreached-search').addEventListener('input', renderUnreached);
  $('#gap-search').addEventListener('input', () => { gapLimit = 100; renderGaps(); });
  $('#gap-filter').addEventListener('change', () => { gapLimit = 100; renderGaps(); });

  let inventoryLimit = 100;
  function renderInventory() {
    const search = $('#inventory-search').value.trim().toLowerCase(); const filter = $('#inventory-filter').value;
    const filtered = report.callsite_inventory.callsites.filter(site => (filter === 'all' || site.status === filter) && (!search || `${loc(site.location)} ${site.reason}`.toLowerCase().includes(search)));
    $('#inventory-count').textContent = `${filtered.length} of ${report.callsite_inventory.callsites.length} · ${report.callsite_inventory.configured_files} configured files`;
    const list = $('#inventory-list'); list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching callsites.')); return; }
    filtered.slice(0, inventoryLimit).forEach(site => { const row = node('div', 'inventory-row'); const content = node('div'); content.append(node('div', 'mono', loc(site.location)), node('div', 'aside', site.reason)); row.append(badge(site.status), content); list.append(row); });
    if (filtered.length > inventoryLimit) list.append(button(`Show more (${filtered.length - inventoryLimit} remaining)`, () => { inventoryLimit += 100; renderInventory(); }, 'load'));
  }
  $('#inventory-search').addEventListener('input', () => { inventoryLimit = 100; renderInventory(); });
  $('#inventory-filter').addEventListener('change', () => { inventoryLimit = 100; renderInventory(); });

  let evidenceLimit = 100;
  function renderEvidence() {
    const search = $('#evidence-search').value.trim().toLowerCase();
    const filtered = report.evidence.filter(item => !search || `E${item.id} ${item.rule} ${item.summary}`.toLowerCase().includes(search));
    $('#evidence-count').textContent = `${filtered.length} of ${report.evidence.length}`;
    const list = $('#evidence-list'); list.replaceChildren();
    if (!filtered.length) { list.append(node('p', 'empty', 'No matching evidence.')); return; }
    filtered.slice(0, evidenceLimit).forEach(item => list.append(evidenceNode(item)));
    if (filtered.length > evidenceLimit) list.append(button(`Show more (${filtered.length - evidenceLimit} remaining)`, () => { evidenceLimit += 100; renderEvidence(); }, 'load'));
  }
  function evidenceNode(item) {
    const element = node('div', 'evidence-node');
    element.append(node('div', 'mono', `E${item.id} · ${item.relation} · ${item.rule}`), node('div', '', item.summary));
    if (item.parents?.length) element.append(node('div', 'aside', `Parents: ${item.parents.map(id => `E${id}`).join(', ')}`));
    return element;
  }
  function showEvidence(id) {
    showTab('evidence');
    $('#evidence-search').value = '';
    const list = $('#evidence-list'); list.replaceChildren();
    const heading = node('div'); heading.append(node('h2', '', `Evidence path from ${id}`), button('Show all evidence', () => renderEvidence())); list.append(heading);
    let current = id; const visited = new Set();
    while (current && !visited.has(current) && visited.size < 64) {
      visited.add(current); const item = evidenceById.get(current);
      if (!item) break;
      list.append(evidenceNode(item));
      current = item.parents?.length ? `E${item.parents[0]}` : null;
    }
    $('#evidence-count').textContent = `${visited.size} nodes on selected path`;
  }
  $('#evidence-search').addEventListener('input', () => { evidenceLimit = 100; renderEvidence(); });
  renderCallsites(); renderCreationList(); renderCreationDetail(); renderBoundaries(); renderUnreached(); renderGaps(); renderInventory(); renderEvidence();
})();
