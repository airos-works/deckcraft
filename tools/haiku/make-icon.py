#!/usr/bin/env python3
"""Convert DeckCraft's original SVG rectangles/circle into a 64-unit Haiku vector icon."""
from pathlib import Path
import xml.etree.ElementTree as ET
import hvif

root = Path(__file__).resolve().parents[2]
svg = ET.parse(root / 'assets/app-icon/hicolor/scalable/apps/ai.storyteller.deckcraft.svg')
icon = {'styles': [], 'paths': [], 'shapes': []}

def rounded(x, y, w, h, r):
    k = r * 0.55228475
    return [((x+r,y),(x+r-k,y),(x+r,y)), ((x+w-r,y),(x+w-r,y),(x+w-r+k,y)),
            ((x+w,y+r),(x+w,y+r-k),(x+w,y+r)), ((x+w,y+h-r),(x+w,y+h-r),(x+w,y+h-r+k)),
            ((x+w-r,y+h),(x+w-r+k,y+h),(x+w-r,y+h)), ((x+r,y+h),(x+r,y+h),(x+r-k,y+h)),
            ((x,y+h-r),(x,y+h-r+k),(x,y+h-r)), ((x,y+r),(x,y+r),(x,y+r-k))]

for elem in svg.getroot():
    a = elem.attrib
    if elem.tag.endswith('rect'):
        x,y,w,h = [float(a.get(key,0))/8 for key in ('x','y','width','height')]
        r = float(a.get('rx',0))/8
        points = rounded(x,y,w,h,r) if r else [(x,y),(x+w,y),(x+w,y+h),(x,y+h)]
    elif elem.tag.endswith('circle'):
        x,y,r = [float(a[key])/8 for key in ('cx','cy','r')]
        points = rounded(x-r,y-r,2*r,2*r,r)
    else:
        raise ValueError('unsupported SVG element: '+elem.tag)
    path = len(icon['paths']); icon['paths'].append({'closed':True,'points':points})
    for attr in ('fill','stroke'):
        if attr not in a: continue
        style = len(icon['styles']); icon['styles'].append({'color':hvif.hex_color(a[attr])})
        transforms = [] if attr=='fill' else [{'type':'stroke','width':float(a['stroke-width'])/8,'join':2,'cap':0,'miter':4}]
        icon['shapes'].append({'style':style,'paths':[path],'transformers':transforms})
out = root / 'apps/deckcraft-haiku/resources/DeckCraft.hvif'
out.write_bytes(hvif.encode(icon))
print(out)
