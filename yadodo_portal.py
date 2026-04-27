from datetime import datetime
import os

from flask import Flask, flash, redirect, render_template, request, url_for
from flask_sqlalchemy import SQLAlchemy

app = Flask(__name__)
app.config["SECRET_KEY"] = os.getenv("SECRET_KEY", "change_me_in_production")
app.config["SQLALCHEMY_DATABASE_URI"] = "sqlite:///yadodo_clients.db"
app.config["SQLALCHEMY_TRACK_MODIFICATIONS"] = False
app.config["UPLOAD_FOLDER"] = "static/uploads/"

os.makedirs(app.config["UPLOAD_FOLDER"], exist_ok=True)

db = SQLAlchemy(app)


class User(db.Model):
    """Database model for land buyers and next of kin details."""

    id = db.Column(db.Integer, primary_key=True)

    # Personal details
    full_name = db.Column(db.String(100), nullable=False)
    gender = db.Column(db.String(10))
    occupation = db.Column(db.String(100))
    phone = db.Column(db.String(20))
    email = db.Column(db.String(100), unique=True)

    # Geography
    country = db.Column(db.String(50))
    state = db.Column(db.String(50))
    lga = db.Column(db.String(50))
    location = db.Column(db.String(200))
    user_photo = db.Column(db.String(200))

    # Next of kin details
    nok_name = db.Column(db.String(100))
    nok_gender = db.Column(db.String(10))
    nok_phone = db.Column(db.String(20))
    nok_photo = db.Column(db.String(200))

    registration_date = db.Column(db.DateTime, default=datetime.utcnow)


# Utility for 5% vendor fee calculation
def calculate_commission(property_price: float) -> float:
    commission_rate = 0.05
    return property_price * commission_rate


@app.route("/")
def index():
    return "<h1>Welcome to Yadodo Lands & Properties Portal</h1>"


@app.route("/register", methods=["GET", "POST"])
def register():
    if request.method == "POST":
        new_user = User(
            full_name=request.form.get("full_name", "").strip(),
            email=request.form.get("email", "").strip().lower(),
            phone=request.form.get("phone", "").strip(),
            country=request.form.get("country", "").strip(),
            state=request.form.get("state", "").strip(),
            lga=request.form.get("lga", "").strip(),
            nok_name=request.form.get("nok_name", "").strip(),
            # Photo handling logic would go here
        )

        db.session.add(new_user)
        db.session.commit()
        flash("Registration successful! Welcome to Yadodo.")
        return redirect(url_for("index"))

    return render_template("register.html")


if __name__ == "__main__":
    with app.app_context():
        db.create_all()
    app.run(debug=True)
