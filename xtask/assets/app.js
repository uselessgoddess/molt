"use strict";

const REPO = "https://github.com/uselessgoddess/molt";
const runs = JSON.parse(document.getElementById("runs").textContent || "[]");

const BOX = { w: 340, h: 132, top: 10, right: 8, bottom: 18, left: 52 };

function time(ns) {
  if (ns < 1e3) return ns.toFixed(1) + " ns";
  if (ns < 1e6) return (ns / 1e3).toFixed(2) + " µs";
  return (ns / 1e6).toFixed(2) + " ms";
}

function change(now, then) {
  const delta = (now - then) / then * 100;
  const cell = document.createElement("td");
  cell.textContent = (delta >= 0 ? "+" : "") + delta.toFixed(1) + "%";
  cell.className = Math.abs(delta) < 1 ? "flat" : delta < 0 ? "win" : "loss";
  return cell;
}

// Every bench id anyone recorded, so one added late still charts.
function ids() {
  const seen = new Set();
  for (const run of runs) for (const bench of run.benches) seen.add(bench.id);
  return [...seen].sort();
}

function series(id) {
  const points = [];
  runs.forEach((run, index) => {
    const bench = run.benches.find((bench) => bench.id === id);
    if (bench) points.push({ index, run, bench });
  });
  return points;
}

function commit(run) {
  const link = document.createElement("a");
  link.href = REPO + "/commit/" + run.commit.hash;
  link.textContent = run.commit.hash.slice(0, 9);
  return link;
}

function head() {
  const run = runs[runs.length - 1];
  const line = document.getElementById("latest");
  line.textContent = "";
  line.append(commit(run), " ");

  const subject = document.createElement("span");
  subject.className = "subject";
  subject.textContent = run.commit.subject;
  line.append(subject);
  line.append(
    ` · ${run.commit.date.slice(0, 10)} · ${run.machine.cpu}` +
      ` · ${run.machine.arch} ${run.machine.os} · ${run.machine.toolchain}`,
  );
}

function summary() {
  const body = document.querySelector("#benches tbody");
  for (const id of ids()) {
    const points = series(id);
    const last = points[points.length - 1];
    if (last.index !== runs.length - 1) continue;

    const row = document.createElement("tr");
    const name = document.createElement("td");
    name.className = "name";
    name.textContent = id;
    row.append(name, cell(time(last.bench.median)), cell("±" + time(last.bench.spread)));
    row.append(
      points.length > 1 ? change(last.bench.median, points[points.length - 2].bench.median) : cell("—"),
      points.length > 1 ? change(last.bench.median, points[0].bench.median) : cell("—"),
    );
    body.append(row);
  }
}

function cell(text) {
  const cell = document.createElement("td");
  cell.textContent = text;
  return cell;
}

function svg(id, zero) {
  const points = series(id);
  const top = BOX.top, left = BOX.left;
  const width = BOX.w - left - BOX.right;
  const height = BOX.h - top - BOX.bottom;

  let hi = Math.max(...points.map((point) => point.bench.high));
  let lo = zero ? 0 : Math.min(...points.map((point) => point.bench.low));
  if (!(hi > lo)) [lo, hi] = [0, hi * 1.2 || 1];
  const margin = (hi - lo) * 0.08;
  hi += margin;
  if (!zero) lo = Math.max(0, lo - margin);

  const x = (index) => left + (points.length < 2 ? width / 2 : width * index / (points.length - 1));
  const y = (value) => top + height - height * (value - lo) / (hi - lo);

  let out = "";
  for (const level of [lo, (lo + hi) / 2, hi]) {
    out += `<line class="grid" x1="${left}" y1="${y(level)}" x2="${left + width}" y2="${y(level)}"/>`;
    out += `<text class="tick" x="${left - 6}" y="${y(level) + 3}" text-anchor="end">${time(level)}</text>`;
  }

  const up = points.map((point, index) => `${x(index)},${y(point.bench.high)}`);
  const down = points.map((point, index) => `${x(index)},${y(point.bench.low)}`).reverse();
  out += `<polygon class="band" points="${up.concat(down).join(" ")}"/>`;
  out += `<polyline class="line" points="${points.map((point, index) => `${x(index)},${y(point.bench.median)}`).join(" ")}"/>`;

  points.forEach((point, index) => {
    const label = `${point.run.commit.hash.slice(0, 9)} ${point.run.commit.subject}\n${time(point.bench.median)}`;
    out += `<a href="${REPO}/commit/${point.run.commit.hash}" target="_blank">` +
      `<circle class="dot" cx="${x(index)}" cy="${y(point.bench.median)}" r="2.5"><title>${plain(label)}</title></circle></a>`;
  });
  return `<svg viewBox="0 0 ${BOX.w} ${BOX.h}" preserveAspectRatio="none">${out}</svg>`;
}

function plain(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function charts() {
  const zero = document.getElementById("zero").checked;
  const board = document.getElementById("charts");
  board.textContent = "";
  for (const id of ids()) {
    const points = series(id);
    const figure = document.createElement("figure");
    const caption = document.createElement("figcaption");
    caption.append(id);

    const value = document.createElement("span");
    value.textContent = time(points[points.length - 1].bench.median);
    caption.append(value);
    figure.append(caption);
    figure.insertAdjacentHTML("beforeend", svg(id, zero));
    board.append(figure);
  }
}

if (runs.length) {
  head();
  summary();
  charts();
  document.getElementById("zero").addEventListener("change", charts);
}
