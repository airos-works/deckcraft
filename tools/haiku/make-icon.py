#!/usr/bin/env python3
"""Original Haiku presentation screen. Rebuild with python3 tools/haiku/make-icon.py."""
from icon_art import Drawing, INK, plane

d = Drawing()
p = plane(7, 7, 0.25)
d.shadow([(18,51),(34,59),(61,59),(46,51)])
# Tripod, with floor-contact edges in the BeOS 2:1 / 1:1 directions.
d.line([(29,38),(29,54)], INK, 4)
d.line([(29,51),(17,57)], INK, 3)
d.line([(29,51),(43,58)], INK, 3)
d.line([(29,51),(37,43)], INK, 3)
d.line([(29,39),(29,51)], 'c4cbd1', 1.8)
d.shape([(47,16),(51,12),(51,42),(47,46)], d.gradient('b3a196','6d5e59'), INK)
d.shape(p([(0,0),(40,0),(40,29),(0,29)]), d.gradient('fffff4','dddcd5'), INK)
d.shape(p([(-1,-1),(41,-1),(41,3),(-1,3)]), d.gradient('f5cc98','b97a46'), INK)
d.shape(p([(-1,27),(41,27),(41,30),(-1,30)]), d.gradient('d0c4b5','8e8176'), INK, 1.5)
d.line(p([(5,8),(24,8)]), 'b36c40', 2.5)
for u, v in [(6,18),(15,14),(24,10)]:
    d.shape(p([(u,v),(u+6,v),(u+6,24),(u,24)]), d.gradient('f1b57b','bf6b3e'))
d.line(p([(33,7),(33,22)]), '94918b', 1.4)
d.save(__file__, 'deckcraft', 'DeckCraft')
