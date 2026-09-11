(() => {
  const $ = (id) => document.getElementById(id);
  let ws, state = null, clickTimer = null;

  function connect() {
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws/control');
    ws.onopen = () => { $('link').textContent = 'Connecté'; $('link').className = 'pill on'; };
    ws.onclose = () => { $('link').textContent = 'Déconnecté'; $('link').className = 'pill off'; setTimeout(connect, 1500); };
    ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.type === 'state') { state = m.state; render(); }
    };
  }
  const send = (m) => { if (ws && ws.readyState === 1) ws.send(JSON.stringify(m)); };

  function render() {
    if (!state) return;
    const p = $('phone');
    p.textContent = 'Téléphone : ' + (state.phone_connected ? (state.phone_name || 'connecté') : 'absent');
    p.className = 'pill ' + (state.phone_connected ? 'on' : 'off');
    const st = state.stats || {};
    const parts = [];
    if (st.phone_width) parts.push(`reçu ${st.phone_width}x${st.phone_height} ${Math.round(st.phone_fps)} i/s`);
    if (st.rtp) parts.push(`${st.rtp.codec} ${(st.rtp.bitrate_kbps / 1000).toFixed(1)} Mb/s · perte ${st.rtp.loss_percent.toFixed(1)}% · gigue ${Math.round(st.rtp.jitter_ms)} ms · NACK ${st.rtp.nack_count} PLI ${st.rtp.pli_count}` + (st.rtp.rtt_ms != null ? ` · RTT ${Math.round(st.rtp.rtt_ms)} ms` : ''));
    if (st.phone) parts.push(`envoi ${st.phone.width}x${st.phone.height} ${Math.round(st.phone.fps)} i/s ${(st.phone.bitrate_kbps / 1000).toFixed(1)} Mb/s · limite : ${st.phone.quality_limitation}`);
    parts.push(`rendu ${Math.round(st.render_fps)} i/s`);
    $('stats').textContent = parts.join(' · ');
    const box = $('scenes');
    box.innerHTML = '';
    state.scenes.forEach((s, i) => {
      const el = document.createElement('div');
      el.className = 'scene' + (s.id === state.program ? ' program' : '') + (s.id === state.preview ? ' preview' : '');
      el.innerHTML = `<div>${s.name}</div><small>${i + 1} · ${s.id}</small>`;
      el.onclick = () => { clearTimeout(clickTimer); clickTimer = setTimeout(() => send({ type: 'preview', scene: s.id }), 250); };
      el.ondblclick = () => { clearTimeout(clickTimer); send({ type: 'program', scene: s.id }); };
      box.appendChild(el);
    });
  }
  $('take').onclick = () => send({ type: 'take' });
  document.addEventListener('keydown', (e) => {
    if (e.target.tagName === 'INPUT') return;
    if (e.key === 'Enter' || e.key === ' ') send({ type: 'take' });
    const d = parseInt(e.key, 10);
    if (state && d >= 1 && d <= state.scenes.length) send({ type: 'program', scene: state.scenes[d - 1].id });
  });

  // ---- Écran de sélection des sources audio --------------------------------------------
  const ROUTES = [
    { target: 'branding', label: 'Habillage → sortie', field: 'branding', kind: 'sortie' },
    { target: 'stream', label: 'Stream WebRTC → sortie', field: 'stream', kind: 'sortie' },
    { target: 'return', label: 'Retour téléphone ← entrée', field: 'return_input', kind: 'entrée' },
  ];
  async function loadAudio() {
    let a;
    try { a = await (await fetch('/api/audio')).json(); } catch (e) { return; }
    const routes = $('audio-routes');
    routes.innerHTML = '';
    for (const r of ROUTES) {
      const chans = (a.routing[r.field] || []).join(', ');
      const row = document.createElement('div');
      row.className = 'aroute';
      row.innerHTML = `<label>${r.label}</label><input value="${chans}" inputmode="numeric"><button class="small">OK</button>`;
      const input = row.querySelector('input');
      const apply = () => {
        const channels = input.value.split(/[,\s]+/).map((x) => parseInt(x, 10)).filter((x) => x >= 1);
        send({ type: 'audio_route', target: r.target, channels });
      };
      row.querySelector('button').onclick = apply;
      input.onkeydown = (e) => { if (e.key === 'Enter') apply(); };
      routes.appendChild(row);
    }
    const dev = $('audio-devices');
    dev.innerHTML = '';
    (a.devices || []).forEach((d) => {
      const el = document.createElement('div');
      el.textContent = `[${d.direction}] ${d.name}` + (d.channels ? ` — ${d.channels} canaux` : '');
      dev.appendChild(el);
    });
    dev.insertAdjacentHTML('afterbegin', `<div class="hint">Sortie : ${a.output_device} · Entrée : ${a.input_device} · ${a.sample_rate} Hz</div>`);
  }
  $('audio-toggle').onclick = () => {
    const box = $('audio');
    box.hidden = !box.hidden;
    $('audio-toggle').textContent = box.hidden ? 'Configurer ▾' : 'Masquer ▴';
    if (!box.hidden) loadAudio();
  };

  connect();
})();
