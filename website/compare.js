(() => {
  const card = document.querySelector("#compare");
  const play = card.querySelector(".listen");
  const timeline = card.querySelector("#timeline");
  const mix = card.querySelector("#mix");
  const wave = card.querySelector("canvas");
  const clock = card.querySelector("#clock");
  const message = card.querySelector("#ab-status");
  const length = 60;
  let context, buffers, loading, gains, pair, startTime, startOffset = 0;
  let position = 0, generation = 0, frame = 0, levels, pending = false;

  function time(sec) {
    const whole = Math.floor(sec);
    return Math.floor(whole / 60) + ":" + String(whole % 60).padStart(2, "0");
  }
  function now() {
    return pair ? Math.min(length, startOffset + Math.max(0, context.currentTime - startTime)) : position;
  }
  function paint() {
    const sec = now();
    timeline.value = sec;
    timeline.setAttribute("aria-valuetext", time(sec));
    clock.value = time(sec) + " / 1:00";
    const w = wave.clientWidth, h = wave.clientHeight;
    const dpr = window.devicePixelRatio || 1;
    if (wave.width !== Math.round(w*dpr) || wave.height !== Math.round(h*dpr)) {
      wave.width = Math.round(w*dpr); wave.height = Math.round(h*dpr);
    }
    const g = wave.getContext("2d");
    g.setTransform(dpr,0,0,dpr,0,0); g.clearRect(0,0,w,h);
    if (levels) {
      const count = Math.max(1,Math.floor(w/3.3));
      const blend = Number(mix.value)/100;
      for (let i=0;i<count;i++) {
        const col = Math.min(levels[0].length-1, Math.floor(i/count*levels[0].length));
        const amp = levels[0][col]*(1-blend)+levels[1][col]*blend;
        const bar = Math.max(2,amp*(h-5));
        g.fillStyle = i/count < sec/length ? "#f6d6ea" : "#aa99ad";
        g.fillRect(i*w/count, (h-bar)/2, 2, bar);
      }
    }
    g.fillStyle="#fff"; g.fillRect(Math.min(w-2,sec/length*w),0,2,h);
  }
  function tick() {
    paint();
    if (pair) frame=requestAnimationFrame(tick);
  }
  function setPlay(playing) {
    play.setAttribute("aria-label", playing ? "Пауза" : "Воспроизвести");
    play.setAttribute("aria-pressed", String(playing));
    play.firstElementChild.textContent = playing ? "Ⅱ" : "▶";
  }
  function stop() {
    position = now();
    generation++;
    pending = false;
    cancelAnimationFrame(frame);
    if (pair) {
      const old=pair; pair=null;
      old.forEach(src => { src.onended=null; src.stop(); src.disconnect(); });
    }
    setPlay(false);
    paint();
  }
  function balance() {
    const t=Number(mix.value)/100;
    if (gains) gains.forEach((node,i) => {
      const gain=node.gain, value=i===0 ? 1-t : t;
      gain.cancelScheduledValues(context.currentTime);
      if (pair) gain.setTargetAtTime(value,context.currentTime,.009);
      else gain.setValueAtTime(value,context.currentTime);
    });
    card.querySelectorAll("[data-mix]").forEach(b => b.setAttribute("aria-pressed",String(Number(b.dataset.mix)===Number(mix.value))));
    mix.setAttribute("aria-valuetext", t===0 ? "С цензурой" : t===1 ? "Без цензуры" : "Без цензуры "+mix.value+" процентов");
    paint();
  }
  async function load() {
    if (buffers) return;
    if (!loading) {
      loading=Promise.all(["yandex","original"].map(async name => {
        const response=await fetch("assets/groupie/"+name+".m4a");
        if (!response.ok) throw new Error("audio unavailable");
        const buffer=await context.decodeAudioData(await response.arrayBuffer());
        if (buffer.duration < length-.02) throw new Error("incomplete audio");
        return buffer;
      })).then(result => { buffers=result; }).finally(() => { loading=null; });
    }
    await loading;
  }
  async function begin() {
    const id=++generation;
    pending=true;
    try {
      if (!context) {
        const Audio=window.AudioContext || window.webkitAudioContext;
        context=new Audio();
        gains=[context.createGain(),context.createGain()];
        gains.forEach(n=>n.connect(context.destination));
        balance();
      }
      if (!buffers) message.textContent="Загружаем обе версии…";
      await context.resume(); await load();
      if (id!==generation) return;
      position = position>=length-.025 ? 0 : position;
      startOffset=position;
      startTime=context.currentTime+.045;
      pair=buffers.map((buffer,i) => {
        const src=context.createBufferSource();
        src.buffer=buffer; src.connect(gains[i]); return src;
      });
      pair[0].onended=() => {
        if (id!==generation) return;
        const finished=pair; pair=null; finished.forEach(s=>s.disconnect());
        position=length; cancelAnimationFrame(frame); setPlay(false); paint();
      };
      pair.forEach(src=>src.start(startTime,startOffset,length-startOffset));
      pending=false; message.textContent=""; setPlay(true); tick();
    } catch (_) {
      if (id!==generation) return;
      stop(); message.textContent="Не удалось включить фрагмент. Нажми воспроизведение ещё раз.";
    }
  }
  function seek(seconds, autoplay=false) {
    const resume=Boolean(pair)||pending||autoplay;
    stop(); position=Math.max(0,Math.min(length,seconds)); paint();
    if (resume && position<length) begin();
  }
  play.addEventListener("click",()=>{ if(pair||pending){stop();message.textContent="";}else begin(); });
  timeline.addEventListener("input",()=>seek(Number(timeline.value)));
  mix.addEventListener("input",balance);
  card.querySelectorAll("[data-mix]").forEach(b=>b.addEventListener("click",()=>{mix.value=b.dataset.mix;balance();}));
  card.querySelectorAll("[data-time]").forEach(b=>b.addEventListener("click",()=>seek(Number(b.dataset.time),true)));
  fetch("assets/groupie/wave.json").then(r=>r.json()).then(data=>{levels=data;paint();}).catch(()=>{});
  window.addEventListener("resize",paint);
  window.addEventListener("pagehide",stop);
  balance();
})();
