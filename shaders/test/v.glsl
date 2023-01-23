#version 430

in vec3 position;
in vec3 tex_coords;
out vec3 tex;

uniform mat4 pvmat;
uniform mat4 tmat;

void main() {
    tex = tex_coords;
    vec4 world_pos = tmat * vec4(position, 1.0);
    vec4 pos = pvmat * world_pos;
    gl_Position = pos;
}
