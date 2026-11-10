import fs from 'node:fs';
const dir=new URL('./',import.meta.url).pathname;
const R=(x,y,w,h,r,f)=>`<rect x="${x}" y="${y}" width="${w}" height="${h}" rx="${r}" fill="${f}"/>`;
const P=(p,f,extra='')=>`<polygon points="${p.map(v=>v.join(',')).join(' ')}" fill="${f}" ${extra}/>`;
const gradient=(id,stops,x1=0,y1=0,x2=0,y2=1)=>`<linearGradient id="${id}" x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}">${stops.map(([o,c])=>`<stop offset="${o}" stop-color="${c}"/>`).join('')}</linearGradient>`;
const specs=JSON.parse(fs.readFileSync(dir+'specs.json'));
function rails(s){const x=s.left??248,y=s.y??260,right=s.right??776,w=s.weight,c=s.chamfer,seam=s.seam;
 const top=[[right,y],[x+c,y],[x,y+c],[x,512-seam/2],[x+w,512-seam/2],[x+w,y+c+w*(Math.SQRT2-1)],[x+c+w*(Math.SQRT2-1),y+w],[right,y+w]];
 return [top,top.map(([x,y])=>[x,1024-y])];}
function draw(s,flat=false){
 const depth=s.depth;
 const polys=rails(s);const forms=(fill,dx=0,dy=0)=>polys.map(p=>P(p.map(([x,y])=>[x+dx,y+dy]),fill)).join('');
 if(flat)return forms('#131820');
 const a=s.id==='06-porcelain';
 const defs=`<defs>${gradient('rim',[[0,s.rim[0]],[.46,s.rim[1]],[1,s.rim[1]]])}${gradient('tile',[[0,s.tile[0]],[1,s.tile[1]]],0,0,.55,1)}${gradient('face',[[0,s.face[0]],[1,s.face[1]]],0,0,.3,1)}${gradient('edge',[[0,a?'#D1D9E4':'#B9C2D1'],[1,a?'#5D6879':'#465264']])}<radialGradient id="light" cx=".3" cy="0" r="1"><stop stop-color="white" stop-opacity="0.07"/><stop offset=".7" stop-color="white" stop-opacity="0"/></radialGradient><filter id="shadow" x="-30%" y="-30%" width="160%" height="180%"><feDropShadow dx="0" dy="8" stdDeviation="6" flood-color="#000000" flood-opacity="${a?.2:.48}"/></filter><filter id="tile-shadow" x="-20%" y="-20%" width="140%" height="150%"><feDropShadow dx="0" dy="10" stdDeviation="10" flood-opacity=".24"/></filter><clipPath id="face-clip">${forms('#FFFFFF')}</clipPath></defs>`;
 const tile=`<g filter="url(#tile-shadow)">${R(80,80,864,864,194,'url(#rim)')}</g>${R(86,86,852,852,188,'url(#tile)')}${R(86,86,852,852,188,'url(#light)')}`;
 // Extrusion follows one consistent light source; an inset copy leaves a narrow lit upper edge.
 const shaded=depth?`<g filter="url(#shadow)">${forms('url(#edge)',0,depth)}${forms(s.face[0])}<g clip-path="url(#face-clip)">${forms('url(#face)',0,3)}</g></g>`:forms('url(#face)');
 return defs+tile+shaded;
}
const svg=body=>`<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">${body}</svg>`;
for(const s of specs)fs.writeFileSync(dir+s.id+'.svg',svg(draw(s)));
fs.writeFileSync(dir+'figure.svg',svg(draw(specs[0],true)));
