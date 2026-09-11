(() => {
  const $ = (id) => document.getElementById(id);
  let ws, state = null, clickTimer = null;

  function connect() {
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws/control');
    ws.onopen = () => { $('link').textContent = 'Connecté'; $('link').className = 'pill on'; };
    ws.onclose = () => { $('link').textContent = 'Déconnecté'; $('link').className = 'pill off'; setTimeout(connect, 1500); };
    ws.onmessage = (ev) => { const m = JSON.parse(ev.data); if (m.type === 'state') { state = m.state; render(); } };
  }
  const send = (m) => { if (ws && ws.readyState === 1) ws.send(JSON.stringify(m)); };

  function render() {
    if (!state) return;
    const p = $('phone');
    p.textContent = 'Téléphone : ' + (state.phone_connected ? (state.phone_name || 'connecté') : 'absent');
    p.className = 'pill ' + (state.phone_connected ? 'on' : 'off');
    const st = state.stats || {};
    $('stats').textContent = (st.phone_width ? `${st.phone_width}x${st.phone_height} · ${Math.round(st.phone_fps)} i/s · ` : '') + `rendu ${Math.round(st.render_fps)} i/s`;
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
    if (e.key === 'Enter' || e.key === ' ') send({ type: 'take' });
    const d = parseInt(e.key, 10);
    if (state && d >= 1 && d <= state.scenes.length) send({ type: 'program', scene: state.scenes[d - 1].id });
  });
  connect();
})();
