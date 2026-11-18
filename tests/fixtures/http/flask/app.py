from flask import Blueprint, Flask

app = Flask(__name__)
bp = Blueprint("posts", __name__, url_prefix="/posts")


@app.route("/")
def index():
    return "home"


@app.route("/login", methods=["GET", "POST"])
def login():
    return "login"


@bp.route("/<int:post_id>")
def show(post_id):
    return str(post_id)


@bp.delete("/<post_id>")
def remove(post_id):
    return ""


app.register_blueprint(bp)
