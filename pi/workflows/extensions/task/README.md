# Task extension

Registers `/task`, the one command for Deliverable work. A Task's parent is an Epic or none. Initialization requires prepared project configuration and renders the one project-wide format. A standalone Task starts from and targets the repository default; an Epic Task copies its Epic's current Epic or default target into its immutable branch contract. Linear `tracker` uses the exact issue branch without fallback.
